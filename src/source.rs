use std::{
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// LFとタブも、構造として扱う場所を除き必ず可視化する。
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{1b}' => "␛".into(),
            '\0' => "␀".into(),
            c if c.is_control() => format!("\\u{{{:04X}}}", c as u32),
            c => c.to_string(),
        })
        .collect()
}

pub fn load(path: &Path) -> io::Result<String> {
    // FIFOを開く段階で停止しない。開いたFDの属性も検証し、パス置換の競合に備える。
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "通常ファイルではありません",
        ));
    }
    if meta.len() > MAX_BYTES {
        return Err(too_large());
    }
    let bytes = bounded_read(file, MAX_BYTES)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "UTF-8が不正です"))?;
    Ok(text
        .strip_prefix('\u{feff}')
        .unwrap_or(&text)
        .replace("\r\n", "\n"))
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "文書が32 MiBの上限を超えています",
    )
}

pub fn bounded_read(reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        Err(too_large())
    } else {
        Ok(bytes)
    }
}
