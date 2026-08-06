enum Attempt<T> {
    Stable(T),
    Changed,
}

fn read_once(
    access: &FileAccess,
    path: &ResolvedPath,
    absolute: &str,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<Attempt<ToolOutput>, ReadError> {
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let mut file = open_regular(access, path)?;
    let before = FileFingerprint::from_file(&file)?;
    run_before_read_hook();

    let mut prefix = Vec::with_capacity(PREFIX_BYTES);
    file.by_ref()
        .take(PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)?;
    if has_binary_magic(&prefix) {
        return Err(ReadError::Binary);
    }
    let reader = io::Cursor::new(prefix).chain(&mut file);
    let mut collector = LineCollector::new(request.start_line.unwrap_or(1), request.line_count);
    let summary = decode_stream(
        reader,
        request.encoding.as_deref(),
        usize::MAX,
        cancellation,
        |chunk| Ok(collector.push(chunk)),
    )?;
    collector.finish_eof();
    run_after_read_hook();

    let after = FileFingerprint::from_file(&file)?;
    if before != after {
        return Ok(Attempt::Changed);
    }
    let identity = match open_regular(access, path) {
        Ok(identity) => FileFingerprint::from_file(&identity)?,
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Attempt::Changed);
        }
        Err(error) => return Err(error),
    };
    if before != identity {
        return Ok(Attempt::Changed);
    }

    render(
        absolute,
        request,
        summary.source_encoding,
        &collector,
        cancellation,
    )
    .map(Attempt::Stable)
}

fn open_regular(access: &FileAccess, path: &ResolvedPath) -> Result<File, ReadError> {
    let metadata = access.symlink_metadata_kind(path)?;
    if metadata.is_dir {
        return Err(ReadError::Directory);
    }
    if path.is_ambient() && metadata.is_symlink {
        return Err(ReadError::NotRegular);
    }
    if !metadata.is_file && !metadata.is_symlink {
        return Err(ReadError::NotRegular);
    }
    let file = access.open_read(path)?;
    if !FileFingerprint::from_file(&file)?.regular {
        return Err(ReadError::NotRegular);
    }
    Ok(file)
}

fn has_binary_magic(prefix: &[u8]) -> bool {
    prefix.starts_with(b"%PDF-")
        || prefix.starts_with(b"\x7FELF")
        || prefix.starts_with(b"MZ")
        || prefix.starts_with(b"\x89PNG\r\n\x1A\n")
        || prefix.starts_with(&[0xFF, 0xD8, 0xFF])
        || prefix.starts_with(b"GIF87a")
        || prefix.starts_with(b"GIF89a")
        || prefix.starts_with(b"BM")
        || prefix.starts_with(b"II*\0")
        || prefix.starts_with(b"MM\0*")
        || prefix.starts_with(b"RIFF") && prefix.get(8..12) == Some(b"WEBP")
}
