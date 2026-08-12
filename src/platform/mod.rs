pub(crate) mod diagnostics;
pub(crate) mod path;
pub(crate) mod process;

#[cfg(not(any(unix, windows)))]
compile_error!("codexshim supports only Windows and Unix hosts");
