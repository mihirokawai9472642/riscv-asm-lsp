mod common;

#[path = "integration/completion.rs"]
mod completion;
#[path = "integration/diagnostics.rs"]
mod diagnostics;
#[path = "integration/hover.rs"]
mod hover;
#[path = "integration/navigation.rs"]
mod navigation;
#[path = "integration/semantic_tokens.rs"]
mod semantic_tokens;
#[path = "integration/workspace.rs"]
mod workspace;
#[path = "integration/zed_extension.rs"]
mod zed_extension;
