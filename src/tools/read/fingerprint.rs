pub(crate) enum Attempt<T> {
    Stable(T),
    Changed,
}

enum PreparedKind {
    Pdf,
    Text {
        detected_encoding: Option<&'static str>,
    },
}

pub(crate) struct PreparedRead {
    resolved: ResolvedPath,
    absolute: String,
    file: File,
    before: FileFingerprint,
    prefix: Vec<u8>,
    kind: PreparedKind,
}

impl PreparedRead {
    pub(crate) fn memory_charge(&self) -> usize {
        match self.kind {
            PreparedKind::Pdf => PDF_READ_MEMORY_BYTES,
            PreparedKind::Text { .. } => TEXT_READ_MEMORY_BYTES,
        }
    }
}

pub(crate) fn prepare(
    access: &FileAccess,
    request: &ReadRequest,
    cancellation: &CancellationToken,
) -> Result<PreparedRead, ReadError> {
    request.validate()?;
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let resolved = access.resolve(Path::new(&request.path))?;
    let absolute = crate::path::display_path(resolved.absolute());
    let mut file = open_regular(access, &resolved)?;
    let before = FileFingerprint::from_file(&file)?;
    run_before_read_hook();

    let mut prefix = Vec::with_capacity(PREFIX_BYTES);
    file.by_ref()
        .take(PREFIX_BYTES as u64)
        .read_to_end(&mut prefix)?;
    let kind = if has_pdf_header(&prefix) || has_pdf_parameters(request) {
        PreparedKind::Pdf
    } else {
        if has_binary_magic(&prefix) {
            return Err(ReadError::Binary);
        }
        let detected_encoding = detect_legacy_encoding(
            &prefix,
            request.encoding.as_deref(),
            before.length() <= prefix.len() as u64,
        )?;
        PreparedKind::Text { detected_encoding }
    };
    Ok(PreparedRead {
        resolved,
        absolute,
        file,
        before,
        prefix,
        kind,
    })
}

pub(crate) fn execute_prepared(
    access: &FileAccess,
    request: &ReadRequest,
    mut prepared: PreparedRead,
    cancellation: &CancellationToken,
) -> Result<Attempt<ToolOutput>, ReadError> {
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    if matches!(prepared.kind, PreparedKind::Pdf) {
        let output = read_pdf(
            &prepared.file,
            &prepared.absolute,
            request,
            cancellation,
        )?;
        run_after_read_hook();
        return if source_is_unchanged(
            access,
            &prepared.resolved,
            &prepared.file,
            &prepared.before,
        )? {
            Ok(Attempt::Stable(output))
        } else {
            Ok(Attempt::Changed)
        };
    }
    let PreparedKind::Text { detected_encoding } = prepared.kind else {
        unreachable!("PDF reads return before text decoding");
    };
    let reader = io::Cursor::new(prepared.prefix).chain(&mut prepared.file);
    let mut collector = LineCollector::new(request.start_line.unwrap_or(1), request.line_count);
    let summary = decode_stream(
        reader,
        request.encoding.as_deref().or(detected_encoding),
        usize::MAX,
        cancellation,
        |chunk| Ok(collector.push(chunk)),
    )?;
    collector.finish_eof();
    run_after_read_hook();

    if !source_is_unchanged(
        access,
        &prepared.resolved,
        &prepared.file,
        &prepared.before,
    )? {
        return Ok(Attempt::Changed);
    }

    render(
        &prepared.absolute,
        request,
        summary.source_encoding,
        &collector,
        cancellation,
    )
    .map(Attempt::Stable)
}

fn source_is_unchanged(
    access: &FileAccess,
    path: &ResolvedPath,
    file: &File,
    before: &FileFingerprint,
) -> Result<bool, ReadError> {
    if before != &FileFingerprint::from_file(file)? {
        return Ok(false);
    }
    match open_regular(access, path) {
        Ok(identity) => Ok(before == &FileFingerprint::from_file(&identity)?),
        Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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
    prefix.starts_with(b"\x7FELF")
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
