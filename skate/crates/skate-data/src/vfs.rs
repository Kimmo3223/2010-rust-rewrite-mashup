//! The engine's file reads: `std::fs` natively. On the web the asset root is a URL path on the
//! page's origin, read with synchronous requests (the engine runs on a worker, where they may block),
//! and nothing is written.

#[cfg(not(target_arch = "wasm32"))]
pub use std::fs::{create_dir_all, read, read_to_string, write};

/// Whether `path` names a readable file.
#[cfg(not(target_arch = "wasm32"))]
pub fn is_file(path: &std::path::Path) -> bool {
    path.is_file()
}

#[cfg(target_arch = "wasm32")]
pub use web::{create_dir_all, is_file, read, read_to_string, write};

#[cfg(target_arch = "wasm32")]
mod web {
    use std::io;
    use std::path::Path;

    use wasm_bindgen::prelude::*;

    #[wasm_bindgen(inline_js = r#"
    export function skate_get(url, head) {
        const x = new XMLHttpRequest();
        // The engine runs on a worker from a blob: URL, which resolves no root-relative path.
        x.open(head ? 'HEAD' : 'GET', url.startsWith('/') ? location.origin + url : url, false);
        x.overrideMimeType('text/plain; charset=x-user-defined');
        x.send();
        if (x.status !== 200) return undefined;
        if (head) return new Uint8Array(0);
        const s = x.responseText, out = new Uint8Array(s.length);
        for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i) & 0xff;
        return out;
    }
    "#)]
    extern "C" {
        fn skate_get(url: &str, head: bool) -> Option<js_sys::Uint8Array>;
    }

    fn url(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
        let url = url(path.as_ref());
        skate_get(&url, false)
            .map(|b| b.to_vec())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, url))
    }

    pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
        String::from_utf8(read(path)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn is_file(path: &Path) -> bool {
        skate_get(&url(path), true).is_some()
    }

    pub fn write(path: impl AsRef<Path>, _contents: impl AsRef<[u8]>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            url(path.as_ref()),
        ))
    }

    pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            url(path.as_ref()),
        ))
    }
}
