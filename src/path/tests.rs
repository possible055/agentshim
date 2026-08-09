#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use super::{FileAccess, PathError, ReadScope, RepositoryRoot};

    #[test]
    fn read_scope_parsing_is_explicit_and_fail_closed() {
        assert_eq!(
            "normal".parse::<ReadScope>().expect("normal"),
            ReadScope::Normal
        );
        assert_eq!(
            "unrestricted".parse::<ReadScope>().expect("unrestricted"),
            ReadScope::Unrestricted
        );
        assert!("all".parse::<ReadScope>().is_err());
    }

    #[test]
    fn normal_rejects_unmanaged_paths_and_unrestricted_admits_them() {
        let fixture = tempfile::tempdir().expect("root fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::write(outside.path().join("outside.txt"), "outside").expect("outside file");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let normal = FileAccess::new(Arc::clone(&root), ReadScope::Normal);
        let unrestricted = FileAccess::new(root, ReadScope::Unrestricted);
        let outside_file = outside.path().join("outside.txt");

        assert_eq!(
            normal.resolve(&outside_file).unwrap_err(),
            PathError::OutsideRoot
        );
        let resolved = unrestricted.resolve(&outside_file).expect("ambient path");
        assert!(resolved.is_ambient());
        assert_eq!(resolved.absolute(), outside_file);
        assert_eq!(resolved.key(), Path::new("outside.txt"));
        assert_eq!(
            unrestricted.resolve(Path::new("../outside")).unwrap_err(),
            PathError::ParentEscape
        );
    }

    #[test]
    fn normal_admits_only_configured_codex_roots() {
        let repository = tempfile::tempdir().expect("repository fixture");
        let codex = tempfile::tempdir().expect("codex fixture");
        let unmanaged = tempfile::tempdir().expect("unmanaged fixture");
        let skills = codex.path().join("skills");
        fs::create_dir_all(skills.join("example")).expect("skill directory");
        fs::write(skills.join("example/SKILL.md"), "instructions").expect("skill file");
        let skills = fs::canonicalize(skills).expect("canonical skill root");
        fs::write(unmanaged.path().join("secret.txt"), "secret").expect("unmanaged file");
        let access = FileAccess::with_codex_roots(
            Arc::new(RepositoryRoot::open(repository.path()).expect("root")),
            &[skills.as_path()],
        )
        .expect("normal access");

        let skill_root = access.resolve(&skills).expect("skill root");
        let skill = access
            .resolve(&skills.join("example/SKILL.md"))
            .expect("skill file");
        assert!(skill.is_external());
        assert!(!skill.is_ambient());
        assert!(access.metadata_kind(&skill).expect("metadata").is_file);
        let walked = access
            .resolve_walked_entry(
                &skill_root,
                Path::new("example/SKILL.md"),
                skill.absolute(),
            )
            .expect("walked skill entry");
        let external = access
            .resolve_external_entry(&skill_root, skill.absolute())
            .expect("external skill entry");
        assert_eq!(walked, external);
        assert_eq!(walked.capability_key(), external.capability_key());
        assert_eq!(
            external.key(),
            Path::new("example/SKILL.md")
        );
        assert_eq!(
            access
                .resolve(&unmanaged.path().join("secret.txt"))
                .unwrap_err(),
            PathError::OutsideRoot
        );
    }

    #[cfg(windows)]
    #[test]
    fn unrestricted_scope_rejects_non_disk_windows_prefixes() {
        let fixture = tempfile::tempdir().expect("root fixture");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Unrestricted,
        );
        for path in [r"\\server\share\file.rs", r"\\.\PhysicalDrive0"] {
            assert_eq!(
                access.resolve(Path::new(path)).unwrap_err(),
                PathError::UnsupportedLocation
            );
        }
        assert_eq!(
            access.resolve(Path::new(r"C:relative.rs")).unwrap_err(),
            PathError::AmbiguousPrefix
        );
    }

    #[test]
    fn resolves_relative_and_root_absolute_paths() {
        let fixture = tempfile::tempdir().expect("create fixture");
        fs::create_dir(fixture.path().join("src")).expect("create src");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");

        let relative = root.resolve(Path::new("src/./lib.rs")).expect("relative");
        let absolute = root
            .resolve(&root.path().join("src/lib.rs"))
            .expect("absolute");
        assert_eq!(relative.key(), Path::new("src/lib.rs"));
        assert_eq!(relative, absolute);
        assert_eq!(relative.slash_path(), Some("src/lib.rs"));
    }

    #[test]
    fn walked_repository_entries_match_general_resolution() {
        let fixture = tempfile::tempdir().expect("create fixture");
        fs::create_dir(fixture.path().join("src")).expect("create src");
        fs::write(fixture.path().join("src/lib.rs"), "pub fn fixture() {}")
            .expect("write fixture");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        );
        let operation_root = access.resolve(Path::new("src")).expect("operation root");
        let absolute = access.root().path().join("src/lib.rs");

        let walked = access
            .resolve_walked_entry(&operation_root, Path::new("src/lib.rs"), &absolute)
            .expect("walked entry");
        let general = access.resolve(&absolute).expect("general resolution");

        assert_eq!(walked, general);
        assert_eq!(walked.capability_key(), general.capability_key());
        assert_eq!(walked.slash_path(), general.slash_path());
    }

    #[test]
    fn walked_repository_entries_with_curdir_use_normalized_resolution() {
        let fixture = tempfile::tempdir().expect("create fixture");
        fs::create_dir(fixture.path().join("src")).expect("create src");
        fs::write(fixture.path().join("src/lib.rs"), "pub fn fixture() {}")
            .expect("write fixture");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        );
        let operation_root = access.resolve(Path::new(".")).expect("operation root");
        let key = Path::new("./src/lib.rs");
        let walked = access
            .resolve_walked_entry(&operation_root, key, &access.root().path().join(key))
            .expect("walked entry");

        assert_eq!(walked.key(), Path::new("src/lib.rs"));
        assert_eq!(walked.slash_path(), Some("src/lib.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn walked_repository_absolute_path_with_curdir_is_normalized_even_when_key_is_clean() {
        let fixture = tempfile::tempdir().expect("create fixture");
        fs::create_dir(fixture.path().join("src")).expect("create src");
        fs::write(fixture.path().join("src/lib.rs"), "pub fn fixture() {}")
            .expect("write fixture");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        );
        let operation_root = access.resolve(Path::new(".")).expect("operation root");
        let absolute = access.root().path().join("./src/lib.rs");
        let walked = access
            .resolve_walked_entry(&operation_root, Path::new("src/lib.rs"), &absolute)
            .expect("walked entry");

        assert_eq!(walked.absolute(), access.root().path().join("src/lib.rs"));
        assert!(!walked.absolute().to_string_lossy().contains("/./"));
    }

    #[test]
    fn walked_ambient_entries_match_external_resolution() {
        let repository = tempfile::tempdir().expect("repository fixture");
        let outside = tempfile::tempdir().expect("outside fixture");
        fs::create_dir(outside.path().join("nested")).expect("nested directory");
        let absolute = outside.path().join("nested/file.txt");
        fs::write(&absolute, "fixture").expect("fixture file");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(repository.path()).expect("root")),
            ReadScope::Unrestricted,
        );
        let operation_root = access.resolve(outside.path()).expect("operation root");

        let walked = access
            .resolve_walked_entry(&operation_root, Path::new("nested/file.txt"), &absolute)
            .expect("walked entry");
        let general = access
            .resolve_external_entry(&operation_root, &absolute)
            .expect("external resolution");

        assert_eq!(walked, general);
        assert_eq!(walked.slash_path(), Some("nested/file.txt"));
    }

    #[test]
    fn same_parent_batch_does_not_retain_the_directory_after_files_close() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let parent = fixture.path().join("parent");
        let renamed = fixture.path().join("renamed");
        fs::create_dir(&parent).expect("create parent");
        fs::write(parent.join("a.txt"), "a").expect("write a");
        fs::write(parent.join("b.txt"), "b").expect("write b");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        );
        let paths = ["parent/a.txt", "parent/b.txt"]
            .map(|path| access.resolve(Path::new(path)).expect("resolved path"));

        let opened = access
            .open_read_same_parent_batch(&paths)
            .expect("batch parent")
            .into_iter()
            .collect::<std::io::Result<Vec<_>>>()
            .expect("batch files");
        assert_eq!(opened.len(), 2);
        drop(opened);
        fs::rename(&parent, &renamed).expect("parent rename after files close");
        assert!(renamed.join("a.txt").is_file());
    }

    #[test]
    fn same_parent_batch_rejects_mixed_parents() {
        let fixture = tempfile::tempdir().expect("create fixture");
        fs::create_dir(fixture.path().join("a")).expect("create a");
        fs::create_dir(fixture.path().join("b")).expect("create b");
        fs::write(fixture.path().join("a/file.txt"), "a").expect("write a");
        fs::write(fixture.path().join("b/file.txt"), "b").expect("write b");
        let access = FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        );
        let paths = ["a/file.txt", "b/file.txt"]
            .map(|path| access.resolve(Path::new(path)).expect("resolved path"));

        assert_eq!(
            access
                .open_read_same_parent_batch(&paths)
                .expect_err("mixed parents")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn rejects_parent_and_absolute_escape() {
        let fixture = tempfile::tempdir().expect("create fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        assert_eq!(
            root.resolve(Path::new("../outside")).unwrap_err(),
            PathError::ParentEscape
        );
        let outside = fixture
            .path()
            .parent()
            .expect("fixture parent")
            .join("outside");
        assert_eq!(root.resolve(&outside).unwrap_err(), PathError::OutsideRoot);
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_relative_and_case_rules_are_explicit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        assert_eq!(
            root.resolve(Path::new(r"C:ambiguous"))
                .expect_err("drive-relative path"),
            PathError::AmbiguousPrefix
        );

        let absolute = root.path().join("CaseSensitiveName.rs");
        let folded = absolute.to_string_lossy().to_ascii_uppercase();
        let resolved = root
            .resolve(Path::new(&folded))
            .expect("Windows absolute comparison is case-insensitive");
        assert_eq!(resolved.slash_path(), Some("CASESENSITIVENAME.RS"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_standard_and_verbatim_prefixes_are_equivalent() {
        use std::path::PathBuf;

        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        let canonical = root.path().to_string_lossy();
        let standard = canonical
            .strip_prefix(r"\\?\")
            .expect("canonical drive path uses a verbatim prefix");
        let resolved = root
            .resolve(&PathBuf::from(standard).join("CaseSensitiveName.rs"))
            .expect("standard drive path resolves under verbatim root");
        assert_eq!(resolved.slash_path(), Some("CaseSensitiveName.rs"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_prefix_equivalence_is_narrow() {
        use super::windows_component_eq;

        fn prefix(path: &Path) -> std::path::Component<'_> {
            path.components().next().expect("path prefix")
        }

        assert!(windows_component_eq(
            prefix(Path::new(r"C:\repo")),
            prefix(Path::new(r"\\?\c:\repo")),
        ));
        assert!(windows_component_eq(
            prefix(Path::new(r"\\server\share\repo")),
            prefix(Path::new(r"\\?\UNC\SERVER\SHARE\repo")),
        ));
        assert!(!windows_component_eq(
            prefix(Path::new(r"C:\repo")),
            prefix(Path::new(r"D:\repo")),
        ));
        assert!(!windows_component_eq(
            prefix(Path::new(r"\\server\share\repo")),
            prefix(Path::new(r"\\?\UNC\server\other\repo")),
        ));
        assert!(!windows_component_eq(
            prefix(Path::new(r"\\.\COM1")),
            prefix(Path::new(r"\\?\COM1")),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_sort_keys_are_lossless_and_not_model_visible() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let fixture = tempfile::tempdir().expect("create fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("open root");
        let path = std::path::PathBuf::from(OsString::from_vec(vec![b'a', 0xFF]));
        let resolved = root.resolve(&path).expect("resolve raw path");
        assert_eq!(resolved.slash_path(), None);
        assert!(resolved.sort_key() > root.resolve(Path::new("a")).expect("a").sort_key());
    }
}
