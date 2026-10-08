//! Small libzstd adapter for this benchmark example.
//! `ZSTD_LIBRARY` can point to a shared library when the system loader cannot find it.

use std::{
    env,
    ffi::{CStr, c_char, c_int, c_uint, c_void},
    io,
    path::PathBuf,
    process::Command,
};

use libloading::Library;

type CreateContext = unsafe extern "C" fn() -> *mut c_void;
type FreeContext = unsafe extern "C" fn(*mut c_void) -> usize;
type CompressBound = unsafe extern "C" fn(usize) -> usize;
type CompressPlain =
    unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *const c_void, usize, c_int) -> usize;
type CompressDict = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    usize,
    *const c_void,
    usize,
    *const c_void,
    usize,
    c_int,
) -> usize;
type DecompressDict = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    usize,
    *const c_void,
    usize,
    *const c_void,
    usize,
) -> usize;
type DecompressPlain =
    unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *const c_void, usize) -> usize;
type TrainDict =
    unsafe extern "C" fn(*mut c_void, usize, *const c_void, *const usize, c_uint) -> usize;
type IsError = unsafe extern "C" fn(usize) -> c_uint;
type ErrorName = unsafe extern "C" fn(usize) -> *const c_char;

pub struct Zstd {
    // Keep the library loaded while the copied function pointers and context exist.
    _library: Library,
    context: *mut c_void,
    decompress_context: *mut c_void,
    free_context: FreeContext,
    free_decompress_context: FreeContext,
    compress_bound: CompressBound,
    compress_plain: CompressPlain,
    compress_dict: CompressDict,
    decompress_plain: DecompressPlain,
    decompress_dict: DecompressDict,
    train_dict: TrainDict,
    zstd_is_error: IsError,
    zstd_error_name: ErrorName,
    dict_is_error: IsError,
    dict_error_name: ErrorName,
}

impl Zstd {
    pub fn load() -> io::Result<Self> {
        let mut paths = Vec::new();
        if let Some(path) = env::var_os("ZSTD_LIBRARY") {
            paths.push(PathBuf::from(path));
        } else {
            paths.extend(
                ["libzstd.so.1", "libzstd.so", "libzstd.dylib", "zstd.dll"].map(PathBuf::from),
            );
            // Nix places shared libraries outside the system loader's paths.
            if let Ok(output) = Command::new("pkg-config")
                .args(["--variable=libdir", "libzstd"])
                .output()
            {
                if output.status.success() {
                    let dir = String::from_utf8_lossy(&output.stdout);
                    for name in ["libzstd.so.1", "libzstd.so", "libzstd.dylib"] {
                        paths.push(PathBuf::from(dir.trim()).join(name));
                    }
                }
            }
        }
        let library = paths
            .iter()
            .find_map(|path| unsafe { Library::new(path).ok() })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "libzstd not found; set ZSTD_LIBRARY to its shared-library path",
                )
            })?;
        unsafe fn symbol<T: Copy>(library: &Library, name: &[u8]) -> io::Result<T> {
            // The signatures below match the public zstd.h and zdict.h ABI.
            Ok(*unsafe { library.get::<T>(name) }
                .map_err(|error| io::Error::other(error.to_string()))?)
        }
        // Resolve every symbol before allocating the context, so loading errors
        // cannot leak a context.
        let create_context: CreateContext = unsafe { symbol(&library, b"ZSTD_createCCtx")? };
        let create_decompress_context: CreateContext =
            unsafe { symbol(&library, b"ZSTD_createDCtx")? };
        let free_context: FreeContext = unsafe { symbol(&library, b"ZSTD_freeCCtx")? };
        let free_decompress_context: FreeContext = unsafe { symbol(&library, b"ZSTD_freeDCtx")? };
        let compress_bound = unsafe { symbol(&library, b"ZSTD_compressBound")? };
        let compress_plain = unsafe { symbol(&library, b"ZSTD_compressCCtx")? };
        let compress_dict = unsafe { symbol(&library, b"ZSTD_compress_usingDict")? };
        let decompress_plain = unsafe { symbol(&library, b"ZSTD_decompressDCtx")? };
        let decompress_dict = unsafe { symbol(&library, b"ZSTD_decompress_usingDict")? };
        let train_dict = unsafe { symbol(&library, b"ZDICT_trainFromBuffer")? };
        let zstd_is_error = unsafe { symbol(&library, b"ZSTD_isError")? };
        let zstd_error_name = unsafe { symbol(&library, b"ZSTD_getErrorName")? };
        let dict_is_error = unsafe { symbol(&library, b"ZDICT_isError")? };
        let dict_error_name = unsafe { symbol(&library, b"ZDICT_getErrorName")? };
        let context = unsafe { create_context() };
        if context.is_null() {
            return Err(io::Error::other("ZSTD_createCCtx failed"));
        }
        let decompress_context = unsafe { create_decompress_context() };
        if decompress_context.is_null() {
            unsafe { free_context(context) };
            return Err(io::Error::other("ZSTD_createDCtx failed"));
        }
        Ok(Self {
            _library: library,
            context,
            decompress_context,
            free_context,
            free_decompress_context,
            compress_bound,
            compress_plain,
            compress_dict,
            decompress_plain,
            decompress_dict,
            train_dict,
            zstd_is_error,
            zstd_error_name,
            dict_is_error,
            dict_error_name,
        })
    }

    pub fn compress(
        &mut self,
        input: &[u8],
        dictionary: Option<&[u8]>,
        level: i32,
    ) -> io::Result<usize> {
        Ok(self.compress_bytes(input, dictionary, level)?.len())
    }

    pub fn compress_bytes(
        &mut self,
        input: &[u8],
        dictionary: Option<&[u8]>,
        level: i32,
    ) -> io::Result<Vec<u8>> {
        let bound = unsafe { (self.compress_bound)(input.len()) };
        if unsafe { (self.zstd_is_error)(bound) } != 0 {
            return Err(self.zstd_error(bound));
        }
        let mut output = vec![0u8; bound];
        let size = match dictionary {
            Some(dict) => unsafe {
                (self.compress_dict)(
                    self.context,
                    output.as_mut_ptr().cast(),
                    output.len(),
                    input.as_ptr().cast(),
                    input.len(),
                    dict.as_ptr().cast(),
                    dict.len(),
                    level,
                )
            },
            None => unsafe {
                (self.compress_plain)(
                    self.context,
                    output.as_mut_ptr().cast(),
                    output.len(),
                    input.as_ptr().cast(),
                    input.len(),
                    level,
                )
            },
        };
        if unsafe { (self.zstd_is_error)(size) } != 0 {
            return Err(self.zstd_error(size));
        }
        output.truncate(size);
        Ok(output)
    }

    pub fn decompress_with_dict(
        &mut self,
        input: &[u8],
        output_len: usize,
        dictionary: &[u8],
    ) -> io::Result<Vec<u8>> {
        let mut output = vec![0u8; output_len];
        let size = unsafe {
            (self.decompress_dict)(
                self.decompress_context,
                output.as_mut_ptr().cast(),
                output.len(),
                input.as_ptr().cast(),
                input.len(),
                dictionary.as_ptr().cast(),
                dictionary.len(),
            )
        };
        if unsafe { (self.zstd_is_error)(size) } != 0 {
            return Err(self.zstd_error(size));
        }
        output.truncate(size);
        Ok(output)
    }

    pub fn decompress_bytes(&mut self, input: &[u8], output_len: usize) -> io::Result<Vec<u8>> {
        // The caller knows each block's original length, so no frame-size lookup is needed.
        let mut output = vec![0u8; output_len];
        let size = unsafe {
            (self.decompress_plain)(
                self.decompress_context,
                output.as_mut_ptr().cast(),
                output.len(),
                input.as_ptr().cast(),
                input.len(),
            )
        };
        if unsafe { (self.zstd_is_error)(size) } != 0 {
            return Err(self.zstd_error(size));
        }
        output.truncate(size);
        Ok(output)
    }

    pub fn train(&self, strings: &[&[u8]], max_bytes: usize) -> Result<Vec<u8>, String> {
        let source_bytes: usize = strings.iter().map(|string| string.len()).sum();
        // zdict needs several samples and substantially more input than output.
        let capacity = max_bytes.min(source_bytes / 8);
        if strings.len() < 2 || capacity < 256 {
            return Err("insufficient samples".to_owned());
        }
        let sample_count = u32::try_from(strings.len()).map_err(|e| e.to_string())?;
        let samples: Vec<usize> = strings.iter().map(|string| string.len()).collect();
        let source: Vec<u8> = strings
            .iter()
            .flat_map(|string| string.iter().copied())
            .collect();
        let mut dictionary = vec![0u8; capacity];
        let size = unsafe {
            (self.train_dict)(
                dictionary.as_mut_ptr().cast(),
                dictionary.len(),
                source.as_ptr().cast(),
                samples.as_ptr(),
                sample_count,
            )
        };
        if unsafe { (self.dict_is_error)(size) } != 0 {
            let name = unsafe { CStr::from_ptr((self.dict_error_name)(size)) };
            return Err(name.to_string_lossy().into_owned());
        }
        dictionary.truncate(size);
        Ok(dictionary)
    }

    fn zstd_error(&self, code: usize) -> io::Error {
        let name = unsafe { CStr::from_ptr((self.zstd_error_name)(code)) };
        io::Error::other(name.to_string_lossy().into_owned())
    }
}

impl Drop for Zstd {
    fn drop(&mut self) {
        unsafe { (self.free_context)(self.context) };
        unsafe { (self.free_decompress_context)(self.decompress_context) };
    }
}
