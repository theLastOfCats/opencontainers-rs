use pest::Parser;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::ops::Deref;
use std::str::FromStr;

use crate::distribution::RegistryError;
use crate::image::{go, Image, ImageSelector};

#[derive(Debug, Fail)]
#[allow(clippy::large_enum_variant)]
pub enum ManifestError {
    #[fail(display = "JSON Error: {:?}", _0)]
    JsonError(serde_json::Error),

    #[fail(display = "Invalid Schema Version: {}", _0)]
    InvalidSchemaVersion(u64),

    #[fail(display = "Invalid (unknown) Media Type: {}", _0)]
    InvalidMediaType(String),

    #[fail(display = "Parsing digest failed: '{}' ({:?})", _0, _1)]
    DigestParseFailed(String, #[cause] pest::error::Error<Rule>),

    #[fail(display = "Invalid digest algorithm: {}", _0)]
    InvalidDigestAlgorithm(String),

    #[fail(display = "Could not find manifest for current platform")]
    NoMatchingPlatformFound,
}

/// Helper struct to determine Image Manifest Schema.
#[derive(Debug, Deserialize)]
struct ManifestSchemaOnlyV2 {
    #[serde(rename = "schemaVersion")]
    schema: u64,
}

impl ManifestSchemaOnlyV2 {
    // Return the schema version.
    pub fn schema(&self) -> u64 {
        self.schema
    }
}

#[derive(Debug, Deserialize)]
// Helper struct to determine Schema 2 Image Manifest media type
struct ManifestMediaTypeOnlyV2_2 {
    /// The MIME type of the referenced object. This should generally be
    /// `application/vnd.docker.container.image.v1+json`.
    #[serde(rename = "mediaType")]
    media_type: String,
}

impl ManifestMediaTypeOnlyV2_2 {
    // Return the schema version.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }
}

pub trait Layer {
    /// Return the digest of the layer
    fn digest(&self) -> &Digest;

    /// Return the media type of the layer, if available
    fn media_type(&self) -> Option<&LayerMediaType>;
}

impl Layer for Box<dyn Layer> {
    fn digest(&self) -> &Digest {
        self.deref().digest()
    }

    fn media_type(&self) -> Option<&LayerMediaType> {
        self.deref().media_type()
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub enum LayerMediaType {
    // application/vnd.oci.image.layer.v1.tar
    Tar,

    // application/vnd.oci.image.layer.v1.tar+gzip
    // application/vnd.docker.image.rootfs.diff.tar.gzip
    TarGz,

    // application/vnd.oci.image.layer.nondistributable.v1.tar
    NondistributableTar,

    // application/vnd.oci.image.layer.nondistributable.v1.tar+gzip
    // application/vnd.docker.image.rootfs.foreign.diff.tar.gzip
    NondistributableTarGz,

    /// An encountered mediaType that is unknown to the implementation MUST be ignored.
    Other(String),
}

impl LayerMediaType {
    /// Return if a media type is distributable
    pub fn is_distributable(&self) -> bool {
        match self {
            LayerMediaType::Tar => true,
            LayerMediaType::TarGz => true,
            LayerMediaType::NondistributableTar => false,
            LayerMediaType::NondistributableTarGz => false,
            // Regard any other media types as distributable by default
            LayerMediaType::Other(_) => true,
        }
    }

    /// Return if media type is gzipped
    pub fn is_gzipped(&self) -> bool {
        match self {
            LayerMediaType::Tar => false,
            LayerMediaType::TarGz => true,
            LayerMediaType::NondistributableTar => false,
            LayerMediaType::NondistributableTarGz => true,
            // Assume other media types are gzipped.
            LayerMediaType::Other(_) => true,
        }
    }
}

impl std::str::FromStr for LayerMediaType {
    type Err = void::Void;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "application/vnd.oci.image.layer.v1.tar" => LayerMediaType::Tar,
            "application/vnd.oci.image.layer.v1.tar+gzip" => LayerMediaType::TarGz,
            "application/vnd.docker.image.rootfs.diff.tar.gzip" => LayerMediaType::TarGz,
            "application/vnd.oci.image.layer.nondistributable.v1.tar" => {
                LayerMediaType::NondistributableTar
            }
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => {
                LayerMediaType::NondistributableTarGz
            }
            "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => {
                LayerMediaType::NondistributableTarGz
            }
            other => LayerMediaType::Other(other.into()),
        })
    }
}

impl std::fmt::Display for LayerMediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                LayerMediaType::Tar => "application/vnd.oci.image.layer.v1.tar",
                LayerMediaType::TarGz => "application/vnd.oci.image.layer.v1.tar+gzip",
                LayerMediaType::NondistributableTar => {
                    "application/vnd.oci.image.layer.nondistributable.v1.tar"
                }
                LayerMediaType::NondistributableTarGz => {
                    "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
                }
                // Assume other media types are gzipped.
                LayerMediaType::Other(media_type) => media_type,
            }
        )
    }
}

impl<'de> Deserialize<'de> for LayerMediaType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for LayerMediaType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}
/// Enum of Manifest structs for each schema version.
#[derive(Debug)]
pub enum ManifestV2 {
    Schema1(ManifestV2_1),
    Schema2(ManifestV2_2),
    Schema2List(ManifestListV2_2),
}

impl ManifestV2 {
    pub fn layers(&self) -> Result<Box<dyn Iterator<Item = &dyn Layer> + '_>, RegistryError> {
        Ok(match self {
            ManifestV2::Schema1(s1) => Box::new(s1.layers.iter().map(|l| l as &dyn Layer)),
            ManifestV2::Schema2(s2) => Box::new(s2.layers.iter().map(|l| l as &dyn Layer)),
            ManifestV2::Schema2List(_) => unimplemented!(),
        })
    }
}

impl FromStr for ManifestV2 {
    type Err = ManifestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match probe_manifest_v2_schema(s)? {
            ManifestV2Schema::Schema1 => serde_json::from_str(s).map(ManifestV2::Schema1),
            ManifestV2Schema::Schema2 => serde_json::from_str(s).map(ManifestV2::Schema2),
            ManifestV2Schema::Schema2List => serde_json::from_str(s).map(ManifestV2::Schema2List),
        }
        .map_err(ManifestError::JsonError)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
/// Discriminants for ManifestV2
pub enum ManifestV2Schema {
    Schema1,
    Schema2,
    Schema2List,
}

impl From<ManifestV2> for ManifestV2Schema {
    fn from(manifest: ManifestV2) -> Self {
        match manifest {
            ManifestV2::Schema1(_) => ManifestV2Schema::Schema1,
            ManifestV2::Schema2(_) => ManifestV2Schema::Schema2,
            ManifestV2::Schema2List(_) => ManifestV2Schema::Schema2List,
        }
    }
}

impl From<&ManifestV2> for ManifestV2Schema {
    fn from(manifest: &ManifestV2) -> Self {
        match manifest {
            ManifestV2::Schema1(_) => ManifestV2Schema::Schema1,
            ManifestV2::Schema2(_) => ManifestV2Schema::Schema2,
            ManifestV2::Schema2List(_) => ManifestV2Schema::Schema2List,
        }
    }
}

pub fn probe_manifest_v2_schema(data: &str) -> Result<ManifestV2Schema, ManifestError> {
    let manifest: ManifestSchemaOnlyV2 =
        serde_json::from_str(data).map_err(ManifestError::JsonError)?;

    match manifest.schema() {
        1 => return Ok(ManifestV2Schema::Schema1),
        2 => {}
        schema => return Err(ManifestError::InvalidSchemaVersion(schema)),
    };

    let manifest: ManifestMediaTypeOnlyV2_2 =
        serde_json::from_str(data).map_err(ManifestError::JsonError)?;

    let media_type = manifest.media_type();

    #[allow(clippy::or_fun_call)]
    let media_type_split = media_type
        .split('+')
        .next()
        .ok_or(ManifestError::InvalidMediaType(media_type.into()))?;

    match media_type_split {
        "application/vnd.oci.distribution.manifest.v2" => Ok(ManifestV2Schema::Schema2),
        "application/vnd.oci.distribution.manifest.list.v2" => Ok(ManifestV2Schema::Schema2List),
        // Docker seems to be compatible to OCI, so we also support those.
        "application/vnd.docker.distribution.manifest.v2" => Ok(ManifestV2Schema::Schema2),
        "application/vnd.docker.distribution.manifest.list.v2" => Ok(ManifestV2Schema::Schema2List),
        _ => Err(ManifestError::InvalidMediaType(media_type.into())),
    }
}

#[derive(Parser)]
#[grammar = "image/digest.pest"]
struct DigestParser;

/// A digest used for content addressability.
///
/// # Spec
///
/// > A digest is a serialized hash result, consisting of a algorithm and hex
/// > portion. The algorithm identifies the methodology used to calculate the
/// > digest. The hex portion is the hex-encoded result of the hash.
///
/// # Example
///
/// ```
///# use opencontainers::image::manifest::Digest;
/// let digest: Digest = "sha256:6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b".parse()
///     .expect("parsing digest failed!");
/// assert_eq!(&digest.to_string(), "sha256:6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b")
/// ```

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct Digest {
    pub algorithm: DigestAlgorithm,
    pub hex: String,
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}:{}", self.algorithm, self.hex)
    }
}

impl std::str::FromStr for Digest {
    type Err = ManifestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut digest = DigestParser::parse(Rule::digest, s)
            .map_err(|e| ManifestError::DigestParseFailed(s.into(), e))?
            .next()
            .unwrap() // Can never fail because we have at least one result
            .into_inner()
            .map(|t| t.as_str().to_owned());
        let algorithm: DigestAlgorithm = digest.next().unwrap().parse()?;
        let hex = digest.next().unwrap();
        Ok(Self { algorithm, hex })
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Hash)]
pub enum DigestAlgorithm {
    Sha256,
}

impl std::fmt::Display for DigestAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DigestAlgorithm::Sha256 => write!(f, "sha256"),
        }
    }
}

impl std::str::FromStr for DigestAlgorithm {
    type Err = ManifestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "sha256" => Ok(DigestAlgorithm::Sha256),
            other => Err(ManifestError::InvalidDigestAlgorithm(other.into())),
        }
    }
}

impl<'de> Deserialize<'de> for DigestAlgorithm {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl Serialize for DigestAlgorithm {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FsLayerV2_1 {
    #[serde(rename = "blobSum")]
    inner: Digest,
}

impl Layer for FsLayerV2_1 {
    fn digest(&self) -> &Digest {
        &self.inner
    }

    fn media_type(&self) -> Option<&LayerMediaType> {
        // Schema 1 does not include a media type
        None
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct V1Compatibility {
    #[serde(rename = "v1Compatibility")]
    inner: String,
}

/// Image Manifest Version 2, Schema 1
#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestV2_1 {
    #[serde(rename = "schemaVersion")]
    schema: u64,

    name: String,
    tag: String,
    architecture: String,

    #[serde(rename = "fsLayers")]
    layers: Vec<FsLayerV2_1>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ConfigV2_2 {
    /// The MIME type of the referenced object. This should generally be
    /// `application/vnd.docker.container.image.v1+json`.
    #[serde(rename = "mediaType")]
    media_type: String,

    /// The size in bytes of the object.
    ///
    /// This field exists so that a client will have an expected size for the
    /// content before validating. If the length of the retrieved content does
    /// not match the specified length, the content should not be trusted.
    size: usize,

    /// The digest of the content, as defined by the [Registry V2 HTTP API
    /// Specificiation](https://docs.docker.com/registry/spec/api/#digest-parameter).
    digest: Digest,
}

impl ConfigV2_2 {
    pub fn digest(&self) -> &Digest {
        &self.digest
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct LayerV2_2 {
    /// The MIME type of the referenced object.
    ///
    /// This should generally be
    /// `application/vnd.docker.image.rootfs.diff.tar.gzip`. Layers of type
    /// `application/vnd.docker.image.rootfs.foreign.diff.tar.gzip` may be
    /// pulled from a remote location but they should never be pushed.
    #[serde(rename = "mediaType")]
    media_type: LayerMediaType,

    /// The size in bytes of the object
    ///
    /// This field exists so that a client will have an expected size for the
    /// content before validating. If the length of the retrieved content does
    /// not match the specified length, the content should not be trusted.
    size: usize,

    /// The digest of the content, as defined by the [Registry V2 HTTP API
    /// Specificiation](https://docs.docker.com/registry/spec/api/#digest-parameter).
    digest: Digest,

    /// Provides a list of URLs from which the content may be fetched.
    ///
    /// Content should be verified against the digest and size. This field is
    /// optional and uncommon.
    urls: Option<Vec<String>>,
}

impl Layer for LayerV2_2 {
    fn digest(&self) -> &Digest {
        &self.digest
    }

    fn media_type(&self) -> Option<&LayerMediaType> {
        Some(&self.media_type)
    }
}

/// Image Manifest Version 2, Schema 2
#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestV2_2 {
    /// This field specifies the image manifest schema version as an integer.
    ///
    /// This schema uses version 2.
    #[serde(rename = "schemaVersion")]
    pub schema: u64,

    /// The MIME type of the manifest. This should be set to
    /// `application/vnd.docker.distribution.manifest.v2+json`.
    #[serde(rename = "mediaType")]
    pub media_type: String,

    /// The config field references a configuration object for a container, by
    /// digest.
    ///
    /// This configuration item is a JSON blob that the runtime uses to
    /// set up the container. This new schema uses a tweaked version of this
    /// configuration to allow image content-addressability on the daemon side.
    #[serde(rename = "config")]
    pub config: ConfigV2_2,

    /// The layer list is ordered starting from the base image
    ///
    /// (opposite order of schema1).
    pub layers: Vec<LayerV2_2>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestPlatformV2_2 {
    /// The architecture field specifies the CPU architecture, for example
    /// amd64 or ppc64le.
    architecture: go::GoArch,

    /// The os field specifies the operating system, for example linux or
    /// windows.
    os: go::GoOs,

    /// The optional os.version field specifies the operating system version,
    /// for example 10.0.10586.
    #[serde(rename = "os.version")]
    osversion: Option<String>,

    /// The optional os.features field specifies an array of strings, each
    /// listing a required OS feature (for example on Windows win32k).
    #[serde(rename = "os.features")]
    osfeatures: Option<Vec<String>>,

    /// The optional variant field specifies a variant of the CPU, for example
    /// armv6l to specify a particular CPU variant of the ARM CPU.
    variant: Option<String>,

    /// The optional features field specifies an array of strings, each listing
    /// a required CPU feature (for example sse4 or aes).
    features: Option<Vec<String>>,
}

impl ManifestPlatformV2_2 {
    pub fn current_platform_matches(&self) -> bool {
        self.current_arch_matches()
            && self.current_os_matches()
            && self.current_features_match()
            && self.current_variant_matches()
    }

    pub fn current_arch_matches(&self) -> bool {
        let current_arch = std::env::consts::ARCH.parse::<go::GoArch>().ok();
        current_arch == Some(self.architecture)
    }

    pub fn current_os_matches(&self) -> bool {
        let current_os = std::env::consts::OS.parse::<go::GoOs>().ok();
        current_os == Some(self.os)
    }

    pub fn current_osfeatures_match(&self) -> bool {
        // On windows, we should check, whether the win32k driver is installed.
        #[cfg(target_platform = "windows")]
        compile_error!("windows is not supported at the moment!");

        true
    }

    pub fn current_features_match(&self) -> bool {
        // This property is RESERVED for future versions of the spec..
        true
    }

    pub fn current_variant_matches(&self) -> bool {
        // FIXME: on arm, we should really check the arm variant here.
        true
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestListEntryV2_2 {
    /// The MIME type of the referenced object.
    ///
    /// This will generally be `application/vnd.docker.image.manifest.v2+json`,
    /// but it could also be `application/vnd.docker.image.manifest.v1+json`
    /// if the manifest list references a legacy schema-1 manifest.
    #[serde(rename = "mediaType")]
    media_type: String,

    /// The size in bytes of the object
    ///
    /// This field exists so that a client will have an expected size for the
    /// content before validating. If the length of the retrieved content does
    /// not match the specified length, the content should not be trusted.
    size: usize,

    /// The digest of the content, as defined by the [Registry V2 HTTP API
    /// Specificiation](https://docs.docker.com/registry/spec/api/#digest-parameter).
    digest: Digest,

    /// The platform object describes the platform which the image in the
    /// manifest runs on. A full list of valid operating system and architecture
    /// values are listed in the Go language documentation for $GOOS and $GOARCH
    pub platform: ManifestPlatformV2_2,
}

/// Manifest List
///
/// The manifest list is the “fat manifest” which points to specific image
/// manifests for one or more platforms. Its use is optional, and relatively
/// few images will use one of these manifests.
///
/// A client will distinguish a manifest list from an image manifest based on
/// the Content-Type returned in the HTTP response.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestListV2_2 {
    /// This field specifies the image manifest schema version as an integer.
    ///
    /// This schema uses version 2.
    #[serde(rename = "schemaVersion")]
    pub schema: u64,

    /// The MIME type of the manifest list. This should be set to
    /// `application/vnd.docker.distribution.manifest.list.v2+json`.
    media_type: String,

    /// The manifests field contains a list of manifests for specific platforms.
    pub manifests: Vec<ManifestListEntryV2_2>,
}

impl ManifestListV2_2 {
    pub fn get_current_platform_manifest_digest<T>(&self) -> Option<&Digest>
    where
        T: ImageSelector,
    {
        T::select_manifest(self).map(|entry| &entry.digest)
    }

    /// Get a platform manifest for the current platform from a manifest list.
    pub fn get_current_platform_manifest<T>(
        &self,
        image: &Image,
    ) -> Result<ManifestV2_2, RegistryError>
    where
        T: ImageSelector,
    {
        let digest = self
            .get_current_platform_manifest_digest::<T>()
            .ok_or(ManifestError::NoMatchingPlatformFound)
            .map_err(RegistryError::ManifestError)?;

        let url = format!(
            "{}/v2/{}/manifests/{}",
            image.registry.url, image.name, digest
        );

        let blob = image
            .registry
            .get(&url, None)?
            .text()
            .map_err(RegistryError::ReqwestError)?;

        serde_json::from_str(&blob)
            .map_err(ManifestError::JsonError)
            .map_err(RegistryError::ManifestError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_manifest_v1() {
        let test_data = include_str!("test/manifest-v2-1.test.json");

        let manifest: ManifestV2_1 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest");

        assert_eq!(manifest.schema, 1);
        assert_eq!(manifest.name, "hello-world");
        assert_eq!(manifest.tag, "latest");
        assert_eq!(manifest.architecture, "amd64");
        assert_eq!(manifest.layers.len(), 4);
    }

    #[test]
    fn test_manifest_v2() {
        let test_data = include_str!("test/manifest-v2-2.test.json");

        let manifest: ManifestV2_2 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest");

        assert_eq!(manifest.schema, 2);
        assert_eq!(
            manifest.media_type,
            "application/vnd.docker.distribution.manifest.v2+json"
        );

        assert_eq!(
            manifest.config.media_type,
            "application/vnd.docker.container.image.v1+json"
        );
        assert_eq!(manifest.config.size, 7023);
        assert_eq!(manifest.config.digest.algorithm, DigestAlgorithm::Sha256);
        assert_eq!(
            manifest.config.digest.hex,
            "b5b2b2c507a0944348e0303114d8d93aaaa081732b86451d9bce1f432a537bc7"
        );

        assert_eq!(manifest.layers.len(), 3);

        assert_eq!(
            manifest.layers[0],
            LayerV2_2 {
                media_type: LayerMediaType::TarGz,
                size: 32654,
                digest: "sha256:e692418e4cbaf90ca69d05a66403747baa33ee08806650b51fab815ad7fc331f"
                    .parse()
                    .expect("Could not parse reference digest"),
                urls: None,
            }
        );

        assert_eq!(
            manifest.layers[1],
            LayerV2_2 {
                media_type: LayerMediaType::TarGz,
                size: 16724,
                digest: "sha256:3c3a4604a545cdc127456d94e421cd355bca5b528f4a9c1905b15da2eb4a4c6b"
                    .parse()
                    .expect("Could not parse reference digest"),
                urls: None,
            }
        );

        assert_eq!(
            manifest.layers[2],
            LayerV2_2 {
                media_type: LayerMediaType::TarGz,
                size: 73109,
                digest: "sha256:ec4b8955958665577945c89419d1af06b5f7636b4ac3da7f12184802ad867736"
                    .parse()
                    .expect("Could not parse reference digest"),
                urls: None,
            }
        );
    }

    #[test]
    fn test_manifest_list_v2() {
        let test_data = include_str!("test/manifest-list-v2-2.test.json");

        let manifest_list: ManifestListV2_2 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest list");

        assert_eq!(manifest_list.schema, 2);
        assert_eq!(
            manifest_list.media_type,
            "application/vnd.docker.distribution.manifest.list.v2+json"
        );
        assert_eq!(manifest_list.manifests.len(), 2);
    }

    #[test]
    fn test_manifest_schemaonly_schema1() {
        let test_data = include_str!("test/manifest-v2-1.test.json");

        let manifest: ManifestSchemaOnlyV2 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest");

        assert_eq!(manifest.schema(), 1);
    }

    #[test]
    fn test_manifest_schemaonly_schema2() {
        let test_data = include_str!("test/manifest-v2-2.test.json");

        let manifest: ManifestSchemaOnlyV2 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest");

        assert_eq!(manifest.schema(), 2);
    }

    #[test]
    fn test_manifest_schemaonly_schema2_list() {
        let test_data = include_str!("test/manifest-list-v2-2.test.json");

        let manifest: ManifestSchemaOnlyV2 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest");

        assert_eq!(manifest.schema(), 2);
    }

    #[test]
    fn test_manifest_mediatypeonly_schema2() {
        let test_data = include_str!("test/manifest-v2-2.test.json");

        let manifest: ManifestMediaTypeOnlyV2_2 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest");

        assert_eq!(
            manifest.media_type(),
            "application/vnd.docker.distribution.manifest.v2+json"
        );
    }

    #[test]
    fn test_manifest_mediatypeonly_schema2_list() {
        let test_data = include_str!("test/manifest-list-v2-2.test.json");

        let manifest: ManifestMediaTypeOnlyV2_2 =
            serde_json::from_str(test_data).expect("Could not deserialize manifest");

        assert_eq!(
            manifest.media_type(),
            "application/vnd.docker.distribution.manifest.list.v2+json"
        );
    }

    #[test]
    fn test_probe_manifest_schema1() {
        let test_data = include_str!("test/manifest-v2-1.test.json");
        let schema = probe_manifest_v2_schema(test_data).expect("could not probe manifest schema");

        assert_eq!(schema, ManifestV2Schema::Schema1);
    }

    #[test]
    fn test_probe_manifest_schema2() {
        let test_data = include_str!("test/manifest-v2-2.test.json");
        let schema = probe_manifest_v2_schema(test_data).expect("could not probe manifest schema");

        assert_eq!(schema, ManifestV2Schema::Schema2);
    }

    #[test]
    fn test_probe_manifest_schema2_list() {
        let test_data = include_str!("test/manifest-list-v2-2.test.json");
        let schema = probe_manifest_v2_schema(test_data).expect("could not probe manifest schema");

        assert_eq!(schema, ManifestV2Schema::Schema2List);
    }

    #[test]
    fn test_parse_manifest_v2() {
        let test_data = include_str!("test/manifest-v2-1.test.json");
        let manifest: ManifestV2 = test_data
            .parse()
            .expect("Could not parse manifest schema 1");
        assert_eq!(ManifestV2Schema::from(manifest), ManifestV2Schema::Schema1);

        let test_data = include_str!("test/manifest-v2-2.test.json");
        let manifest: ManifestV2 = test_data
            .parse()
            .expect("Could not parse manifest schema 2");
        assert_eq!(ManifestV2Schema::from(manifest), ManifestV2Schema::Schema2);

        let test_data = include_str!("test/manifest-list-v2-2.test.json");
        let manifest: ManifestV2 = test_data
            .parse()
            .expect("Could not parse manifest schema 2 list");
        assert_eq!(
            ManifestV2Schema::from(manifest),
            ManifestV2Schema::Schema2List
        );
    }

    #[test]
    fn test_parse_digest() {
        let test_data = "sha256:6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b";
        let digest: Digest = test_data.parse().expect("Could not parse digest");

        assert_eq!(digest.algorithm, DigestAlgorithm::Sha256);
        assert_eq!(
            digest.hex,
            "6c3c624b58dbbcd3c0dd82b4c53f04194d1247c6eebdaab7c610cf7d66709b3b"
        );
        assert_eq!(&digest.to_string(), test_data)
    }

    #[test]
    fn test_parse_digest_fail() {
        "foobar"
            .parse::<Digest>()
            .expect_err("parsing of string without : succeeded");
        "a::deadbeef"
            .parse::<Digest>()
            .expect_err("digest with multiple : succeeded");
        "sha256:xxxyyyzzz"
            .parse::<Digest>()
            .expect_err("parsing digest with non-hex string succeeded");
    }
}
