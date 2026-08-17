//! Language tooling: everything the real language servers feed into this app, in one
//! folder.

use crate::lsp::diagnostics as diagnostics_view;
#[cfg(test)]
use crate::rail::worktrees::WorktreeItem;
use crate::root::*;
use gpui::{prelude::*, Context, Window};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod completion;
pub mod diagnostics;
pub mod hover;

pub(crate) mod client;
pub(crate) mod completion_popup;
#[cfg(test)]
pub(crate) mod fixtures;
