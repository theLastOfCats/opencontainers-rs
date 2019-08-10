//! Module containing traits that glue together code from the runtime, image and distribution specs.
//!
//! Mostly contains traits that need to be implemented by a consumer of this library, as well as
//! some simplistic implementations.

use crate::image::Image;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

#[derive(Debug, Fail)]
pub enum UnpackError {
    #[fail(display = "Could not read tar entries: {}", _0)]
    GetEntries(std::io::Error),

    #[fail(display = "Could not get entry: {}", _0)]
    GetEntry(std::io::Error),

    #[fail(display = "Could not get entry path: {}", _0)]
    GetEntryPath(std::io::Error),

    #[fail(display = "Could not unpack entry: {}", _0)]
    UnpackEntry(std::io::Error),

    #[fail(display = "Cannot extract files outside of root filesystem: {:?}", _0)]
    AttemptedFilesystemTraversal(std::path::PathBuf),
}

/// Return th path to the file to be deleted, if it is a whiteout file, otherwise None.
///
/// # Examples
/// ```
///# use opencontainers::glue::get_whiteout_path;
///# use std::path::PathBuf;
/// let whiteout = PathBuf::from("a/b/.wh.c");
/// let to_delete = get_whiteout_path(whiteout).unwrap();
/// assert_eq!(to_delete, PathBuf::from("a/b/c"));
/// ```
///
/// If the file is not a whiteout, `None` is returned.AsMut
/// ```
///# use opencontainers::glue::get_whiteout_path;
///# use std::path::PathBuf;
/// let path = PathBuf::from("a/b/c");
/// assert!(get_whiteout_path(path).is_none());
/// ```
pub fn get_whiteout_path<P: AsRef<std::path::Path>>(path: P) -> Option<std::path::PathBuf> {
    #[cfg(unix)]
    return get_whiteout_path_unix(path);

    #[cfg(windows)]
    return get_whiteout_path_windows(path);
}

#[cfg(unix)]
fn get_whiteout_path_unix<P: AsRef<std::path::Path>>(path: P) -> Option<std::path::PathBuf> {
    let filename = path.as_ref().file_name()?;

    let bytes: Vec<u8> = filename.as_bytes().into();
    if !bytes.starts_with(b".wh.") {
        return None;
    }

    let mut path = path.as_ref().to_owned();
    path.set_file_name(std::ffi::OsStr::from_bytes(&bytes[4..]));
    Some(path)
}

#[cfg(windows)]
fn get_whiteout_path_windows<P: AsRef<std::path::Path>>(path: P) -> Option<std::path::PathBuf> {
    let filename = path.as_ref().file_name()?;

    let wide: Vec<u16> = filename.encode_wide().collect();
    let needle: std::ffi::OsString = ".wh.".to_owned().into();
    if !wide.starts_with(&needle.encode_wide().collect::<Vec<u16>>()) {
        return None;
    }

    let mut path = path.as_ref().to_owned();
    path.set_file_name(std::ffi::OsString::from_wide(&wide[4..]));
    Some(path)
}
/// A trait that describes the actions required to create a container's root
/// filesystem from an image.
///
/// # Safety
/// To ensure safe handling of the extraction, implementations of [Unpack::add],
/// [Unpack::whiteout_file] and [Unpack::whiteout_folder] *MUST* ensure that
/// these actions apply only to files within the root filesystem bundle.
///
/// While this can be achieved using [tar::Entry::unpack_in] when implementing
/// [Unpack::add], the whiteout handlers *MUST* take care of this individually.
///
/// Implementations that fail to address this are likely to be vulnerable to
/// file system traversal vulnerabilities due to the possibility of `..` being
/// present in the paths contained in the tarball.
pub trait Unpack {
    /// The main entrypoint for unpacking an image
    ///
    /// This will call [Unpack::apply_layer] for each layer in the image, which
    /// will in turn dispatch the [Unpack::add], [Unpack::whiteout_file] and
    /// [Unpack::whiteout_folder] trait methods for each file or whiteout,
    /// respectively.
    fn unpack(&self, image: &Image) -> Result<(), crate::Error> {
        for layer in image
            .manifest()
            .layers()
            .map_err(crate::Error::RegistryError)?
        {
            let tar = image
                .get_layer(layer)
                .map_err(crate::Error::RegistryError)?;
            self.apply_layer(tar).map_err(crate::Error::UnpackError)?;
        }

        Ok(())
    }

    /// Called in order for each layer in the image
    ///
    /// The default implementation calls the [Unpack::pre_apply] and
    /// [Unpack::post_apply] hooks before and after calling
    /// [Unpack::apply_change] for each entry in the changeset.
    fn apply_layer<R: std::io::Read>(&self, mut layer: tar::Archive<R>) -> Result<(), UnpackError> {
        self.pre_apply()?;

        for entry in layer.entries().map_err(UnpackError::GetEntries)? {
            let entry = entry.map_err(UnpackError::GetEntry)?;
            self.apply_change(entry)?;
        }

        self.post_apply()
    }

    /// Applies a change contained in the tar archive
    ///
    /// Determines the type of change to apply, and dispatches [Unpack::add],
    /// [Unpack::whiteout_file] and [Unpack::whiteout_folder] for this change.
    fn apply_change<R: std::io::Read>(&self, entry: tar::Entry<R>) -> Result<(), UnpackError> {
        let path: std::path::PathBuf = entry.path().map_err(UnpackError::GetEntryPath)?.into();
        if let Some(filename) = path.file_name() {
            if filename == std::ffi::OsStr::new(".wh..wh..opq") {
                // FIXME: Removethe whiteout filename.
                return self.whiteout_folder(path);
            }

            if let Some(whiteout_path) = get_whiteout_path(path) {
                return self.whiteout_file(whiteout_path);
            }
        }

        self.add(entry)
    }

    /// Implement this to handle additions and changes
    ///
    /// # Safety
    /// See the Trait-level documentation for safe implementation notes.
    ///
    /// It is recommended to use [tar::Entry::unpack_in] for safe path handling.
    ///
    /// # Example
    /// ```
    ///# use opencontainers::glue::Unpack;
    ///# use opencontainers::glue::UnpackError;
    /// use std::path::PathBuf;
    /// struct Extractor { root: PathBuf };
    ///
    /// impl Unpack for Extractor {
    ///     fn add<R: std::io::Read>(&self, mut entry: tar::Entry<R>) -> Result<(), UnpackError> {
    ///         let path: std::path::PathBuf = entry.path().map_err(UnpackError::GetEntryPath)?.into();
    ///         if !entry
    ///             .unpack_in(self.root.as_path())
    ///             .map_err(UnpackError::UnpackEntry)?
    ///         {
    ///             return Err(UnpackError::AttemptedFilesystemTraversal(path));
    ///         }

    ///         Ok(())
    ///     }
    ///# fn whiteout_file<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), UnpackError> { Ok(()) }
    ///# fn whiteout_folder<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), UnpackError> { Ok(()) }
    /// }
    /// ```
    fn add<R: std::io::Read>(&self, entry: tar::Entry<R>) -> Result<(), UnpackError>;

    /// Implement this to handle individual file whiteouts
    ///
    /// # Safety
    /// See the Trait-level documentation for safe implementation notes.
    fn whiteout_file<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), UnpackError>;

    /// Implement this to whiteout a complete folder
    ///
    /// # Safety
    /// See the Trait-level documentation for safe implementation notes.
    fn whiteout_folder<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), UnpackError>;

    /// Called before applying a layer
    ///
    /// Can be overriden to provide a callback before extraction of the files,
    /// e.g. to create a snapshot or copy of the existing state.
    fn pre_apply(&self) -> Result<(), UnpackError> {
        Ok(())
    }

    /// Called after applying a layer
    ///
    /// Can be overriden to provide a callback after extraction of the files,
    /// e.g. to create a snapshot or copy of the newly extracted state.
    fn post_apply(&self) -> Result<(), UnpackError> {
        Ok(())
    }
}

/// Extracts an image to a folder.
#[derive(Debug, Clone)]
pub struct SimpleFolderUnpacker {
    /// Path to the folder the image will be extracted to.
    root: std::path::PathBuf,
}

impl SimpleFolderUnpacker {
    /// Construct a new SimpleFolderUnpacker, given an existing path.
    pub fn new<P>(path: P) -> Self
    where
        P: Into<std::path::PathBuf>,
    {
        Self { root: path.into() }
    }
}

impl Unpack for SimpleFolderUnpacker {
    fn add<R: std::io::Read>(&self, mut entry: tar::Entry<R>) -> Result<(), UnpackError> {
        let path: std::path::PathBuf = entry.path().map_err(UnpackError::GetEntryPath)?.into();
        // Apply addition or modification:
        // Additions and Modifications are represented the same in the changeset tar archive.
        if !entry
            .unpack_in(self.root.as_path())
            .map_err(UnpackError::UnpackEntry)?
        {
            return Err(UnpackError::AttemptedFilesystemTraversal(path));
        }

        Ok(())
    }

    fn whiteout_file<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), UnpackError> {
        // FIXME: implement whiteout_file.
        println!("whiteout file: {}", path.as_ref().to_string_lossy());
        Ok(())
    }

    fn whiteout_folder<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), UnpackError> {
        // FIXME: implement whiteout_folder.
        println!("whiteout file: {}", path.as_ref().to_string_lossy());
        Ok(())
    }
}