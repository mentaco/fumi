use std::{
    fs::OpenOptions,
    io::{self, Read},
    os::{
        fd::{AsRawFd, BorrowedFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
};

pub const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// 標準入力の表示名（仕様2.4節）。
pub const STDIN_NAME: &str = "[stdin]";
/// 標準入力の読込中に割り込みを確かめる間隔。
const INTERRUPT_POLL_MS: i32 = 50;

/// LFとタブも、構造として扱う場所を除き必ず可視化する。
pub fn sanitize(text: &str) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\u{1b}' => result.push('␛'),
            '\0' => result.push('␀'),
            c if c.is_control() => {
                let _ = write!(result, "\\u{{{:04X}}}", c as u32);
            }
            c => result.push(c),
        }
    }
    result
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
    decode(bounded_read(file, MAX_BYTES)?)
}

/// 読み込んだバイト列を文書の本文にする。入力元に関わらず同じ規則を使う（仕様3節）。
/// 不正なUTF-8は置換せずエラーにし、先頭のBOMを除いてCRLFをLFへ正規化する。
pub fn decode(bytes: Vec<u8>) -> io::Result<String> {
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "UTF-8が不正です"))?;
    Ok(text
        .strip_prefix('\u{feff}')
        .unwrap_or(&text)
        .replace("\r\n", "\n"))
}

/// 標準入力をEOFまで読む（仕様2.4節）。上限・BOM・改行・不正UTF-8はファイルと同じに扱う。
pub fn load_stdin(interrupted: impl Fn() -> bool) -> io::Result<String> {
    // fd 0は起動時に開いていることを確認済み。読み終えるまで所有者はこの関数だけ。
    let stdin = unsafe { BorrowedFd::borrow_raw(0) };
    decode(read_stream(stdin, MAX_BYTES, interrupted)?)
}

/// パイプ等を `limit` バイトまでEOFまで読む。書き手が止まっていても、
/// `interrupted` が真になれば `Interrupted` で戻る。
pub fn read_stream(
    input: BorrowedFd,
    limit: u64,
    interrupted: impl Fn() -> bool,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        if interrupted() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "読込を中断しました",
            ));
        }
        let mut fd = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, INTERRUPT_POLL_MS) };
        if ready == 0 {
            continue;
        }
        let count = if ready < 0 {
            -1
        } else {
            unsafe { libc::read(input.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) }
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..count as usize]);
        if bytes.len() as u64 > limit {
            return Err(too_large());
        }
    }
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
