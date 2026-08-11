mod access;
mod resolved;

pub(crate) use access::SameParentReader;
pub use access::{FileAccess, ReadScope, RepositoryRoot};
pub(crate) use resolved::display_path;
#[cfg(test)]
pub(crate) use resolved::slash_path;
pub use resolved::{PathError, PathSortKey, ResolvedPath};

#[cfg(test)]
#[path = "path/tests.rs"]
mod test_suite;
