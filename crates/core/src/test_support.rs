use std::{path::Path, sync::Arc};

use crate::path::{FileAccess, ReadScope, RepositoryRoot};

/// Builds the shared read harness for one directory at the default scope.
pub fn access(path: &Path) -> Arc<FileAccess> {
    access_with_scope(path, ReadScope::Normal)
}

/// Builds the shared read harness for one directory at an explicit scope.
pub fn access_with_scope(path: &Path, scope: ReadScope) -> Arc<FileAccess> {
    Arc::new(FileAccess::new(
        Arc::new(RepositoryRoot::open(path).expect("open repository root")),
        scope,
    ))
}
