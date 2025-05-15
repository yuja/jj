// Copyright 2021 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(missing_docs)]

use std::any::Any;
use std::collections::hash_map;
use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use itertools::Itertools as _;
use once_cell::sync::Lazy;
use thiserror::Error;

use crate::backend::BackendError;
use crate::backend::ChangeId;
use crate::backend::CommitId;
use crate::commit::Commit;
use crate::dsl_util;
use crate::dsl_util::collect_similar;
use crate::dsl_util::AliasExpandError as _;
use crate::fileset;
use crate::fileset::FilesetDiagnostics;
use crate::fileset::FilesetExpression;
use crate::graph::GraphNode;
use crate::hex_util::to_forward_hex;
use crate::id_prefix::IdPrefixContext;
use crate::id_prefix::IdPrefixIndex;
use crate::object_id::HexPrefix;
use crate::object_id::PrefixResolution;
use crate::op_store::RemoteRefState;
use crate::op_walk;
use crate::ref_name::RemoteRefSymbol;
use crate::ref_name::RemoteRefSymbolBuf;
use crate::ref_name::WorkspaceName;
use crate::ref_name::WorkspaceNameBuf;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo;
use crate::repo::RepoLoaderError;
use crate::repo_path::RepoPathUiConverter;
use crate::revset_parser;
pub use crate::revset_parser::expect_literal;
pub use crate::revset_parser::parse_program;
pub use crate::revset_parser::parse_symbol;
pub use crate::revset_parser::BinaryOp;
pub use crate::revset_parser::ExpressionKind;
pub use crate::revset_parser::ExpressionNode;
pub use crate::revset_parser::FunctionCallNode;
pub use crate::revset_parser::RevsetAliasesMap;
pub use crate::revset_parser::RevsetDiagnostics;
pub use crate::revset_parser::RevsetParseError;
pub use crate::revset_parser::RevsetParseErrorKind;
pub use crate::revset_parser::UnaryOp;
use crate::store::Store;
use crate::str_util::StringPattern;
use crate::time_util::DatePattern;
use crate::time_util::DatePatternContext;

/// Error occurred during symbol resolution.
#[derive(Debug, Error)]
pub enum RevsetResolutionError {
    #[error("Revision `{name}` doesn't exist")]
    NoSuchRevision {
        name: String,
        candidates: Vec<String>,
    },
    #[error("Workspace `{}` doesn't have a working-copy commit", name.as_symbol())]
    WorkspaceMissingWorkingCopy { name: WorkspaceNameBuf },
    #[error("An empty string is not a valid revision")]
    EmptyString,
    #[error("Commit ID prefix `{0}` is ambiguous")]
    AmbiguousCommitIdPrefix(String),
    #[error("Change ID prefix `{0}` is ambiguous")]
    AmbiguousChangeIdPrefix(String),
    #[error("Unexpected error from commit backend")]
    Backend(#[source] BackendError),
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// Error occurred during revset evaluation.
#[derive(Debug, Error)]
pub enum RevsetEvaluationError {
    #[error("Unexpected error from commit backend")]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl RevsetEvaluationError {
    // TODO: Create a higher-level error instead of putting non-BackendErrors in a
    // BackendError
    pub fn into_backend_error(self) -> BackendError {
        match self {
            Self::Backend(err) => err,
            Self::Other(err) => BackendError::Other(err),
        }
    }
}

// assumes index has less than u64::MAX entries.
pub const GENERATION_RANGE_FULL: Range<u64> = 0..u64::MAX;
pub const GENERATION_RANGE_EMPTY: Range<u64> = 0..0;

/// Global flag applied to the entire expression.
///
/// The core revset engine doesn't use this value. It's up to caller to
/// interpret it to change the evaluation behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevsetModifier {
    /// Expression can be evaluated to multiple revisions even if a single
    /// revision is expected by default.
    All,
}

/// Symbol or function to be resolved to `CommitId`s.
#[derive(Clone, Debug)]
pub enum RevsetCommitRef {
    WorkingCopy(WorkspaceNameBuf),
    WorkingCopies,
    Symbol(String),
    RemoteSymbol(RemoteRefSymbolBuf),
    Bookmarks(StringPattern),
    RemoteBookmarks {
        bookmark_pattern: StringPattern,
        remote_pattern: StringPattern,
        remote_ref_state: Option<RemoteRefState>,
    },
    Tags(StringPattern),
    GitRefs,
    GitHead,
}

/// A custom revset filter expression, defined by an extension.
pub trait RevsetFilterExtension: std::fmt::Debug + Any {
    fn as_any(&self) -> &dyn Any;

    /// Returns true iff this filter matches the specified commit.
    fn matches_commit(&self, commit: &Commit) -> bool;
}

#[derive(Clone, Debug)]
pub enum RevsetFilterPredicate {
    /// Commits with number of parents in the range.
    ParentCount(Range<u32>),
    /// Commits with description matching the pattern.
    Description(StringPattern),
    /// Commits with first line of the description matching the pattern.
    Subject(StringPattern),
    /// Commits with author name matching the pattern.
    AuthorName(StringPattern),
    /// Commits with author email matching the pattern.
    AuthorEmail(StringPattern),
    /// Commits with author dates matching the given date pattern.
    AuthorDate(DatePattern),
    /// Commits with committer name matching the pattern.
    CommitterName(StringPattern),
    /// Commits with committer email matching the pattern.
    CommitterEmail(StringPattern),
    /// Commits with committer dates matching the given date pattern.
    CommitterDate(DatePattern),
    /// Commits modifying the paths specified by the fileset.
    File(FilesetExpression),
    /// Commits containing diffs matching the `text` pattern within the `files`.
    DiffContains {
        text: StringPattern,
        files: FilesetExpression,
    },
    /// Commits with conflicts
    HasConflict,
    /// Commits that are cryptographically signed.
    Signed,
    /// Custom predicates provided by extensions
    Extension(Rc<dyn RevsetFilterExtension>),
}

mod private {
    /// Defines [`RevsetExpression`] variants depending on resolution state.
    pub trait ExpressionState {
        type CommitRef: Clone;
        type Operation: Clone;
    }

    // Not constructible because these state types just define associated types.
    #[derive(Debug)]
    pub enum UserExpressionState {}
    #[derive(Debug)]
    pub enum ResolvedExpressionState {}
}

use private::ExpressionState;
use private::ResolvedExpressionState;
use private::UserExpressionState;

impl ExpressionState for UserExpressionState {
    type CommitRef = RevsetCommitRef;
    type Operation = String;
}

impl ExpressionState for ResolvedExpressionState {
    type CommitRef = Infallible;
    type Operation = Infallible;
}

/// [`RevsetExpression`] that may contain unresolved commit refs.
pub type UserRevsetExpression = RevsetExpression<UserExpressionState>;
/// [`RevsetExpression`] that never contains unresolved commit refs.
pub type ResolvedRevsetExpression = RevsetExpression<ResolvedExpressionState>;

/// Tree of revset expressions describing DAG operations.
///
/// Use [`UserRevsetExpression`] or [`ResolvedRevsetExpression`] to construct
/// expression of that state.
#[derive(Clone, Debug)]
pub enum RevsetExpression<St: ExpressionState> {
    None,
    All,
    VisibleHeads,
    Root,
    Commits(Vec<CommitId>),
    CommitRef(St::CommitRef),
    Ancestors {
        heads: Rc<Self>,
        generation: Range<u64>,
    },
    Descendants {
        roots: Rc<Self>,
        generation: Range<u64>,
    },
    // Commits that are ancestors of "heads" but not ancestors of "roots"
    Range {
        roots: Rc<Self>,
        heads: Rc<Self>,
        generation: Range<u64>,
    },
    // Commits that are descendants of "roots" and ancestors of "heads"
    DagRange {
        roots: Rc<Self>,
        heads: Rc<Self>,
        // TODO: maybe add generation_from_roots/heads?
    },
    // Commits reachable from "sources" within "domain"
    Reachable {
        sources: Rc<Self>,
        domain: Rc<Self>,
    },
    Heads(Rc<Self>),
    Roots(Rc<Self>),
    ForkPoint(Rc<Self>),
    Predecessors(Rc<Self>),
    Latest {
        candidates: Rc<Self>,
        count: usize,
    },
    Filter(RevsetFilterPredicate),
    /// Marker for subtree that should be intersected as filter.
    AsFilter(Rc<Self>),
    /// Resolves symbols and visibility at the specified operation.
    AtOperation {
        operation: St::Operation,
        candidates: Rc<Self>,
    },
    /// Resolves visibility within the specified repo state.
    WithinVisibility {
        candidates: Rc<Self>,
        /// Copy of `repo.view().heads()` at the operation.
        visible_heads: Vec<CommitId>,
    },
    Coalesce(Rc<Self>, Rc<Self>),
    Present(Rc<Self>),
    NotIn(Rc<Self>),
    Union(Rc<Self>, Rc<Self>),
    Intersection(Rc<Self>, Rc<Self>),
    Difference(Rc<Self>, Rc<Self>),
}

// Leaf expression that never contains unresolved commit refs, which can be
// either user or resolved expression
impl<St: ExpressionState> RevsetExpression<St> {
    pub fn none() -> Rc<Self> {
        Rc::new(Self::None)
    }

    pub fn all() -> Rc<Self> {
        Rc::new(Self::All)
    }

    pub fn visible_heads() -> Rc<Self> {
        Rc::new(Self::VisibleHeads)
    }

    pub fn root() -> Rc<Self> {
        Rc::new(Self::Root)
    }

    pub fn commit(commit_id: CommitId) -> Rc<Self> {
        Self::commits(vec![commit_id])
    }

    pub fn commits(commit_ids: Vec<CommitId>) -> Rc<Self> {
        Rc::new(Self::Commits(commit_ids))
    }

    pub fn filter(predicate: RevsetFilterPredicate) -> Rc<Self> {
        Rc::new(Self::Filter(predicate))
    }

    /// Find any empty commits.
    pub fn is_empty() -> Rc<Self> {
        Self::filter(RevsetFilterPredicate::File(FilesetExpression::all())).negated()
    }
}

// Leaf expression that represents unresolved commit refs
impl<St: ExpressionState<CommitRef = RevsetCommitRef>> RevsetExpression<St> {
    pub fn working_copy(name: WorkspaceNameBuf) -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::WorkingCopy(name)))
    }

    pub fn working_copies() -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::WorkingCopies))
    }

    pub fn symbol(value: String) -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::Symbol(value)))
    }

    pub fn remote_symbol(value: RemoteRefSymbolBuf) -> Rc<Self> {
        let commit_ref = RevsetCommitRef::RemoteSymbol(value);
        Rc::new(Self::CommitRef(commit_ref))
    }

    pub fn bookmarks(pattern: StringPattern) -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::Bookmarks(pattern)))
    }

    pub fn remote_bookmarks(
        bookmark_pattern: StringPattern,
        remote_pattern: StringPattern,
        remote_ref_state: Option<RemoteRefState>,
    ) -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::RemoteBookmarks {
            bookmark_pattern,
            remote_pattern,
            remote_ref_state,
        }))
    }

    pub fn tags(pattern: StringPattern) -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::Tags(pattern)))
    }

    pub fn git_refs() -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::GitRefs))
    }

    pub fn git_head() -> Rc<Self> {
        Rc::new(Self::CommitRef(RevsetCommitRef::GitHead))
    }
}

// Compound expression
impl<St: ExpressionState> RevsetExpression<St> {
    pub fn latest(self: &Rc<Self>, count: usize) -> Rc<Self> {
        Rc::new(Self::Latest {
            candidates: self.clone(),
            count,
        })
    }

    /// Commits in `self` that don't have descendants in `self`.
    pub fn heads(self: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Heads(self.clone()))
    }

    /// Commits in `self` that don't have ancestors in `self`.
    pub fn roots(self: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Roots(self.clone()))
    }

    /// Parents of `self`.
    pub fn parents(self: &Rc<Self>) -> Rc<Self> {
        self.ancestors_at(1)
    }

    /// Ancestors of `self`, including `self`.
    pub fn ancestors(self: &Rc<Self>) -> Rc<Self> {
        self.ancestors_range(GENERATION_RANGE_FULL)
    }

    /// Ancestors of `self` at an offset of `generation` behind `self`.
    /// The `generation` offset is zero-based starting from `self`.
    pub fn ancestors_at(self: &Rc<Self>, generation: u64) -> Rc<Self> {
        self.ancestors_range(generation..(generation + 1))
    }

    /// Ancestors of `self` in the given range.
    pub fn ancestors_range(self: &Rc<Self>, generation_range: Range<u64>) -> Rc<Self> {
        Rc::new(Self::Ancestors {
            heads: self.clone(),
            generation: generation_range,
        })
    }

    /// Children of `self`.
    pub fn children(self: &Rc<Self>) -> Rc<Self> {
        self.descendants_at(1)
    }

    /// Descendants of `self`, including `self`.
    pub fn descendants(self: &Rc<Self>) -> Rc<Self> {
        self.descendants_range(GENERATION_RANGE_FULL)
    }

    /// Descendants of `self` at an offset of `generation` ahead of `self`.
    /// The `generation` offset is zero-based starting from `self`.
    pub fn descendants_at(self: &Rc<Self>, generation: u64) -> Rc<Self> {
        self.descendants_range(generation..(generation + 1))
    }

    /// Descendants of `self` in the given range.
    pub fn descendants_range(self: &Rc<Self>, generation_range: Range<u64>) -> Rc<Self> {
        Rc::new(Self::Descendants {
            roots: self.clone(),
            generation: generation_range,
        })
    }

    /// Fork point (best common ancestors) of `self`.
    pub fn fork_point(self: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::ForkPoint(self.clone()))
    }

    /// Filter all commits by `predicate` in `self`.
    pub fn filtered(self: &Rc<Self>, predicate: RevsetFilterPredicate) -> Rc<Self> {
        self.intersection(&Self::filter(predicate))
    }

    /// Commits that are descendants of `self` and ancestors of `heads`, both
    /// inclusive.
    pub fn dag_range_to(self: &Rc<Self>, heads: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::DagRange {
            roots: self.clone(),
            heads: heads.clone(),
        })
    }

    /// Connects any ancestors and descendants in the set by adding the commits
    /// between them.
    pub fn connected(self: &Rc<Self>) -> Rc<Self> {
        self.dag_range_to(self)
    }

    /// All commits within `domain` reachable from this set of commits, by
    /// traversing either parent or child edges.
    pub fn reachable(self: &Rc<Self>, domain: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Reachable {
            sources: self.clone(),
            domain: domain.clone(),
        })
    }

    /// Commits reachable from `heads` but not from `self`.
    pub fn range(self: &Rc<Self>, heads: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Range {
            roots: self.clone(),
            heads: heads.clone(),
            generation: GENERATION_RANGE_FULL,
        })
    }

    /// Predecessors of `self`, including `self`.
    pub fn predecessors(self: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Predecessors(self.clone()))
    }

    /// Suppresses name resolution error within `self`.
    pub fn present(self: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Present(self.clone()))
    }

    /// Commits that are not in `self`, i.e. the complement of `self`.
    pub fn negated(self: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::NotIn(self.clone()))
    }

    /// Commits that are in `self` or in `other` (or both).
    pub fn union(self: &Rc<Self>, other: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Union(self.clone(), other.clone()))
    }

    /// Commits that are in any of the `expressions`.
    pub fn union_all(expressions: &[Rc<Self>]) -> Rc<Self> {
        match expressions {
            [] => Self::none(),
            [expression] => expression.clone(),
            _ => {
                // Build balanced tree to minimize the recursion depth.
                let (left, right) = expressions.split_at(expressions.len() / 2);
                Self::union(&Self::union_all(left), &Self::union_all(right))
            }
        }
    }

    /// Commits that are in `self` and in `other`.
    pub fn intersection(self: &Rc<Self>, other: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Intersection(self.clone(), other.clone()))
    }

    /// Commits that are in `self` but not in `other`.
    pub fn minus(self: &Rc<Self>, other: &Rc<Self>) -> Rc<Self> {
        Rc::new(Self::Difference(self.clone(), other.clone()))
    }

    /// Commits that are in the first expression in `expressions` that is not
    /// `none()`.
    pub fn coalesce(expressions: &[Rc<Self>]) -> Rc<Self> {
        match expressions {
            [] => Self::none(),
            [expression] => expression.clone(),
            _ => {
                // Build balanced tree to minimize the recursion depth.
                let (left, right) = expressions.split_at(expressions.len() / 2);
                Rc::new(Self::Coalesce(Self::coalesce(left), Self::coalesce(right)))
            }
        }
    }
}

impl<St: ExpressionState<CommitRef = RevsetCommitRef>> RevsetExpression<St> {
    /// Returns symbol string if this expression is of that type.
    pub fn as_symbol(&self) -> Option<&str> {
        match self {
            RevsetExpression::CommitRef(RevsetCommitRef::Symbol(name)) => Some(name),
            _ => None,
        }
    }
}

impl UserRevsetExpression {
    /// Resolve a user-provided expression. Symbols will be resolved using the
    /// provided `SymbolResolver`.
    pub fn resolve_user_expression(
        &self,
        repo: &dyn Repo,
        symbol_resolver: &dyn SymbolResolver,
    ) -> Result<Rc<ResolvedRevsetExpression>, RevsetResolutionError> {
        resolve_symbols(repo, self, symbol_resolver)
    }
}

impl ResolvedRevsetExpression {
    /// Optimizes and evaluates this expression.
    pub fn evaluate<'index>(
        self: Rc<Self>,
        repo: &'index dyn Repo,
    ) -> Result<Box<dyn Revset + 'index>, RevsetEvaluationError> {
        optimize(self).evaluate_unoptimized(repo)
    }

    /// Evaluates this expression without optimizing it.
    ///
    /// Use this function if `self` is already optimized, or to debug
    /// optimization pass.
    pub fn evaluate_unoptimized<'index>(
        &self,
        repo: &'index dyn Repo,
    ) -> Result<Box<dyn Revset + 'index>, RevsetEvaluationError> {
        let expr = self.to_backend_expression(repo);
        repo.index().evaluate_revset(&expr, repo.store())
    }

    /// Transforms this expression to the form which the `Index` backend will
    /// process.
    pub fn to_backend_expression(&self, repo: &dyn Repo) -> ResolvedExpression {
        resolve_visibility(repo, self)
    }
}

#[derive(Clone, Debug)]
pub enum ResolvedPredicateExpression {
    /// Pure filter predicate.
    Filter(RevsetFilterPredicate),
    /// Set expression to be evaluated as filter. This is typically a subtree
    /// node of `Union` with a pure filter predicate.
    Set(Box<ResolvedExpression>),
    NotIn(Box<ResolvedPredicateExpression>),
    Union(
        Box<ResolvedPredicateExpression>,
        Box<ResolvedPredicateExpression>,
    ),
}

/// Describes evaluation plan of revset expression.
///
/// Unlike `RevsetExpression`, this doesn't contain unresolved symbols or `View`
/// properties.
///
/// Use `RevsetExpression` API to build a query programmatically.
// TODO: rename to BackendExpression?
#[derive(Clone, Debug)]
pub enum ResolvedExpression {
    Commits(Vec<CommitId>),
    Ancestors {
        heads: Box<Self>,
        generation: Range<u64>,
    },
    /// Commits that are ancestors of `heads` but not ancestors of `roots`.
    Range {
        roots: Box<Self>,
        heads: Box<Self>,
        generation: Range<u64>,
    },
    /// Commits that are descendants of `roots` and ancestors of `heads`.
    DagRange {
        roots: Box<Self>,
        heads: Box<Self>,
        generation_from_roots: Range<u64>,
    },
    /// Commits reachable from `sources` within `domain`.
    Reachable {
        sources: Box<Self>,
        domain: Box<Self>,
    },
    Heads(Box<Self>),
    Roots(Box<Self>),
    ForkPoint(Box<Self>),
    Predecessors(Box<Self>),
    Latest {
        candidates: Box<Self>,
        count: usize,
    },
    Coalesce(Box<Self>, Box<Self>),
    Union(Box<Self>, Box<Self>),
    /// Intersects `candidates` with `predicate` by filtering.
    FilterWithin {
        candidates: Box<Self>,
        predicate: ResolvedPredicateExpression,
    },
    /// Intersects expressions by merging.
    Intersection(Box<Self>, Box<Self>),
    Difference(Box<Self>, Box<Self>),
}

pub type RevsetFunction = fn(
    &mut RevsetDiagnostics,
    &FunctionCallNode,
    &LoweringContext,
) -> Result<Rc<UserRevsetExpression>, RevsetParseError>;

static BUILTIN_FUNCTION_MAP: Lazy<HashMap<&'static str, RevsetFunction>> = Lazy::new(|| {
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map: HashMap<&'static str, RevsetFunction> = HashMap::new();
    map.insert("parents", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let expression = lower_expression(diagnostics, arg, context)?;
        Ok(expression.parents())
    });
    map.insert("children", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let expression = lower_expression(diagnostics, arg, context)?;
        Ok(expression.children())
    });
    map.insert("ancestors", |diagnostics, function, context| {
        let ([heads_arg], [depth_opt_arg]) = function.expect_arguments()?;
        let heads = lower_expression(diagnostics, heads_arg, context)?;
        let generation = if let Some(depth_arg) = depth_opt_arg {
            let depth = expect_literal(diagnostics, "integer", depth_arg)?;
            0..depth
        } else {
            GENERATION_RANGE_FULL
        };
        Ok(heads.ancestors_range(generation))
    });
    map.insert("descendants", |diagnostics, function, context| {
        let ([roots_arg], [depth_opt_arg]) = function.expect_arguments()?;
        let roots = lower_expression(diagnostics, roots_arg, context)?;
        let generation = if let Some(depth_arg) = depth_opt_arg {
            let depth = expect_literal(diagnostics, "integer", depth_arg)?;
            0..depth
        } else {
            GENERATION_RANGE_FULL
        };
        Ok(roots.descendants_range(generation))
    });
    map.insert("connected", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let candidates = lower_expression(diagnostics, arg, context)?;
        Ok(candidates.connected())
    });
    map.insert("reachable", |diagnostics, function, context| {
        let [source_arg, domain_arg] = function.expect_exact_arguments()?;
        let sources = lower_expression(diagnostics, source_arg, context)?;
        let domain = lower_expression(diagnostics, domain_arg, context)?;
        Ok(sources.reachable(&domain))
    });
    map.insert("predecessors", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let expression = lower_expression(diagnostics, arg, context)?;
        Ok(expression.predecessors())
    });
    map.insert("none", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::none())
    });
    map.insert("all", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::all())
    });
    map.insert("working_copies", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::working_copies())
    });
    map.insert("heads", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let candidates = lower_expression(diagnostics, arg, context)?;
        Ok(candidates.heads())
    });
    map.insert("roots", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let candidates = lower_expression(diagnostics, arg, context)?;
        Ok(candidates.roots())
    });
    map.insert("visible_heads", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::visible_heads())
    });
    map.insert("root", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::root())
    });
    map.insert("bookmarks", |diagnostics, function, _context| {
        let ([], [opt_arg]) = function.expect_arguments()?;
        let pattern = if let Some(arg) = opt_arg {
            expect_string_pattern(diagnostics, arg)?
        } else {
            StringPattern::everything()
        };
        Ok(RevsetExpression::bookmarks(pattern))
    });
    map.insert("remote_bookmarks", |diagnostics, function, _context| {
        parse_remote_bookmarks_arguments(diagnostics, function, None)
    });
    map.insert(
        "tracked_remote_bookmarks",
        |diagnostics, function, _context| {
            parse_remote_bookmarks_arguments(diagnostics, function, Some(RemoteRefState::Tracked))
        },
    );
    map.insert(
        "untracked_remote_bookmarks",
        |diagnostics, function, _context| {
            parse_remote_bookmarks_arguments(diagnostics, function, Some(RemoteRefState::New))
        },
    );
    map.insert("tags", |diagnostics, function, _context| {
        let ([], [opt_arg]) = function.expect_arguments()?;
        let pattern = if let Some(arg) = opt_arg {
            expect_string_pattern(diagnostics, arg)?
        } else {
            StringPattern::everything()
        };
        Ok(RevsetExpression::tags(pattern))
    });
    map.insert("git_refs", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::git_refs())
    });
    map.insert("git_head", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::git_head())
    });
    map.insert("latest", |diagnostics, function, context| {
        let ([candidates_arg], [count_opt_arg]) = function.expect_arguments()?;
        let candidates = lower_expression(diagnostics, candidates_arg, context)?;
        let count = if let Some(count_arg) = count_opt_arg {
            expect_literal(diagnostics, "integer", count_arg)?
        } else {
            1
        };
        Ok(candidates.latest(count))
    });
    map.insert("fork_point", |diagnostics, function, context| {
        let [expression_arg] = function.expect_exact_arguments()?;
        let expression = lower_expression(diagnostics, expression_arg, context)?;
        Ok(RevsetExpression::fork_point(&expression))
    });
    map.insert("merges", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::filter(
            RevsetFilterPredicate::ParentCount(2..u32::MAX),
        ))
    });
    map.insert("description", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        Ok(RevsetExpression::filter(
            RevsetFilterPredicate::Description(pattern),
        ))
    });
    map.insert("subject", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        let predicate = RevsetFilterPredicate::Subject(pattern);
        Ok(RevsetExpression::filter(predicate))
    });
    map.insert("author", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        let name_predicate = RevsetFilterPredicate::AuthorName(pattern.clone());
        let email_predicate = RevsetFilterPredicate::AuthorEmail(pattern);
        Ok(RevsetExpression::filter(name_predicate)
            .union(&RevsetExpression::filter(email_predicate)))
    });
    map.insert("author_name", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        let predicate = RevsetFilterPredicate::AuthorName(pattern);
        Ok(RevsetExpression::filter(predicate))
    });
    map.insert("author_email", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        let predicate = RevsetFilterPredicate::AuthorEmail(pattern);
        Ok(RevsetExpression::filter(predicate))
    });
    map.insert("author_date", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_date_pattern(diagnostics, arg, context.date_pattern_context())?;
        Ok(RevsetExpression::filter(RevsetFilterPredicate::AuthorDate(
            pattern,
        )))
    });
    map.insert("signed", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        let predicate = RevsetFilterPredicate::Signed;
        Ok(RevsetExpression::filter(predicate))
    });
    map.insert("mine", |_diagnostics, function, context| {
        function.expect_no_arguments()?;
        // Email address domains are inherently case‐insensitive, and the local‐parts
        // are generally (although not universally) treated as case‐insensitive too, so
        // we use a case‐insensitive match here.
        let predicate =
            RevsetFilterPredicate::AuthorEmail(StringPattern::exact_i(context.user_email));
        Ok(RevsetExpression::filter(predicate))
    });
    map.insert("committer", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        let name_predicate = RevsetFilterPredicate::CommitterName(pattern.clone());
        let email_predicate = RevsetFilterPredicate::CommitterEmail(pattern);
        Ok(RevsetExpression::filter(name_predicate)
            .union(&RevsetExpression::filter(email_predicate)))
    });
    map.insert("committer_name", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        let predicate = RevsetFilterPredicate::CommitterName(pattern);
        Ok(RevsetExpression::filter(predicate))
    });
    map.insert("committer_email", |diagnostics, function, _context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_string_pattern(diagnostics, arg)?;
        let predicate = RevsetFilterPredicate::CommitterEmail(pattern);
        Ok(RevsetExpression::filter(predicate))
    });
    map.insert("committer_date", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let pattern = expect_date_pattern(diagnostics, arg, context.date_pattern_context())?;
        Ok(RevsetExpression::filter(
            RevsetFilterPredicate::CommitterDate(pattern),
        ))
    });
    map.insert("empty", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::is_empty())
    });
    map.insert("files", |diagnostics, function, context| {
        let ctx = context.workspace.as_ref().ok_or_else(|| {
            RevsetParseError::with_span(
                RevsetParseErrorKind::FsPathWithoutWorkspace,
                function.args_span, // TODO: better to use name_span?
            )
        })?;
        let [arg] = function.expect_exact_arguments()?;
        let expr = expect_fileset_expression(diagnostics, arg, ctx.path_converter)?;
        Ok(RevsetExpression::filter(RevsetFilterPredicate::File(expr)))
    });
    map.insert("diff_contains", |diagnostics, function, context| {
        let ([text_arg], [files_opt_arg]) = function.expect_arguments()?;
        let text = expect_string_pattern(diagnostics, text_arg)?;
        let files = if let Some(files_arg) = files_opt_arg {
            let ctx = context.workspace.as_ref().ok_or_else(|| {
                RevsetParseError::with_span(
                    RevsetParseErrorKind::FsPathWithoutWorkspace,
                    files_arg.span,
                )
            })?;
            expect_fileset_expression(diagnostics, files_arg, ctx.path_converter)?
        } else {
            // TODO: defaults to CLI path arguments?
            // https://github.com/jj-vcs/jj/issues/2933#issuecomment-1925870731
            FilesetExpression::all()
        };
        Ok(RevsetExpression::filter(
            RevsetFilterPredicate::DiffContains { text, files },
        ))
    });
    map.insert("conflicts", |_diagnostics, function, _context| {
        function.expect_no_arguments()?;
        Ok(RevsetExpression::filter(RevsetFilterPredicate::HasConflict))
    });
    map.insert("present", |diagnostics, function, context| {
        let [arg] = function.expect_exact_arguments()?;
        let expression = lower_expression(diagnostics, arg, context)?;
        Ok(expression.present())
    });
    map.insert("at_operation", |diagnostics, function, context| {
        let [op_arg, cand_arg] = function.expect_exact_arguments()?;
        // TODO: Parse "opset" here if we add proper language support.
        let operation =
            revset_parser::expect_expression_with(diagnostics, op_arg, |_diagnostics, node| {
                Ok(node.span.as_str().to_owned())
            })?;
        let candidates = lower_expression(diagnostics, cand_arg, context)?;
        Ok(Rc::new(RevsetExpression::AtOperation {
            operation,
            candidates,
        }))
    });
    map.insert("coalesce", |diagnostics, function, context| {
        let ([], args) = function.expect_some_arguments()?;
        let expressions: Vec<_> = args
            .iter()
            .map(|arg| lower_expression(diagnostics, arg, context))
            .try_collect()?;
        Ok(RevsetExpression::coalesce(&expressions))
    });
    map
});

/// Parses the given `node` as a fileset expression.
pub fn expect_fileset_expression(
    diagnostics: &mut RevsetDiagnostics,
    node: &ExpressionNode,
    path_converter: &RepoPathUiConverter,
) -> Result<FilesetExpression, RevsetParseError> {
    // Alias handling is a bit tricky. The outermost expression `alias` is
    // substituted, but inner expressions `x & alias` aren't. If this seemed
    // weird, we can either transform AST or turn off revset aliases completely.
    revset_parser::expect_expression_with(diagnostics, node, |diagnostics, node| {
        let mut inner_diagnostics = FilesetDiagnostics::new();
        let expression = fileset::parse(&mut inner_diagnostics, node.span.as_str(), path_converter)
            .map_err(|err| {
                RevsetParseError::expression("In fileset expression", node.span).with_source(err)
            })?;
        diagnostics.extend_with(inner_diagnostics, |diag| {
            RevsetParseError::expression("In fileset expression", node.span).with_source(diag)
        });
        Ok(expression)
    })
}

pub fn expect_string_pattern(
    diagnostics: &mut RevsetDiagnostics,
    node: &ExpressionNode,
) -> Result<StringPattern, RevsetParseError> {
    revset_parser::expect_pattern_with(
        diagnostics,
        "string pattern",
        node,
        |_diagnostics, value, kind| match kind {
            Some(kind) => StringPattern::from_str_kind(value, kind),
            None => Ok(StringPattern::Substring(value.to_owned())),
        },
    )
}

pub fn expect_date_pattern(
    diagnostics: &mut RevsetDiagnostics,
    node: &ExpressionNode,
    context: &DatePatternContext,
) -> Result<DatePattern, RevsetParseError> {
    revset_parser::expect_pattern_with(
        diagnostics,
        "date pattern",
        node,
        |_diagnostics, value, kind| -> Result<_, Box<dyn std::error::Error + Send + Sync>> {
            match kind {
                None => Err("Date pattern must specify 'after' or 'before'".into()),
                Some(kind) => Ok(context.parse_relative(value, kind)?),
            }
        },
    )
}

fn parse_remote_bookmarks_arguments(
    diagnostics: &mut RevsetDiagnostics,
    function: &FunctionCallNode,
    remote_ref_state: Option<RemoteRefState>,
) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
    let ([], [bookmark_opt_arg, remote_opt_arg]) =
        function.expect_named_arguments(&["", "remote"])?;
    let bookmark_pattern = if let Some(bookmark_arg) = bookmark_opt_arg {
        expect_string_pattern(diagnostics, bookmark_arg)?
    } else {
        StringPattern::everything()
    };
    let remote_pattern = if let Some(remote_arg) = remote_opt_arg {
        expect_string_pattern(diagnostics, remote_arg)?
    } else {
        StringPattern::everything()
    };
    Ok(RevsetExpression::remote_bookmarks(
        bookmark_pattern,
        remote_pattern,
        remote_ref_state,
    ))
}

/// Resolves function call by using the given function map.
fn lower_function_call(
    diagnostics: &mut RevsetDiagnostics,
    function: &FunctionCallNode,
    context: &LoweringContext,
) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
    let function_map = &context.extensions.function_map;
    if let Some(func) = function_map.get(function.name) {
        func(diagnostics, function, context)
    } else {
        Err(RevsetParseError::with_span(
            RevsetParseErrorKind::NoSuchFunction {
                name: function.name.to_owned(),
                candidates: collect_similar(function.name, function_map.keys()),
            },
            function.name_span,
        ))
    }
}

/// Transforms the given AST `node` into expression that describes DAG
/// operation. Function calls will be resolved at this stage.
pub fn lower_expression(
    diagnostics: &mut RevsetDiagnostics,
    node: &ExpressionNode,
    context: &LoweringContext,
) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
    match &node.kind {
        ExpressionKind::Identifier(name) => Ok(RevsetExpression::symbol((*name).to_owned())),
        ExpressionKind::String(name) => Ok(RevsetExpression::symbol(name.to_owned())),
        ExpressionKind::StringPattern { .. } => Err(RevsetParseError::with_span(
            RevsetParseErrorKind::NotInfixOperator {
                op: ":".to_owned(),
                similar_op: "::".to_owned(),
                description: "DAG range".to_owned(),
            },
            node.span,
        )),
        ExpressionKind::RemoteSymbol(symbol) => Ok(RevsetExpression::remote_symbol(symbol.clone())),
        ExpressionKind::AtWorkspace(name) => Ok(RevsetExpression::working_copy(name.into())),
        ExpressionKind::AtCurrentWorkspace => {
            let ctx = context.workspace.as_ref().ok_or_else(|| {
                RevsetParseError::with_span(
                    RevsetParseErrorKind::WorkingCopyWithoutWorkspace,
                    node.span,
                )
            })?;
            Ok(RevsetExpression::working_copy(
                ctx.workspace_name.to_owned(),
            ))
        }
        ExpressionKind::DagRangeAll => Ok(RevsetExpression::all()),
        ExpressionKind::RangeAll => {
            Ok(RevsetExpression::root().range(&RevsetExpression::visible_heads()))
        }
        ExpressionKind::Unary(op, arg_node) => {
            let arg = lower_expression(diagnostics, arg_node, context)?;
            match op {
                UnaryOp::Negate => Ok(arg.negated()),
                UnaryOp::DagRangePre => Ok(arg.ancestors()),
                UnaryOp::DagRangePost => Ok(arg.descendants()),
                UnaryOp::RangePre => Ok(RevsetExpression::root().range(&arg)),
                UnaryOp::RangePost => Ok(arg.range(&RevsetExpression::visible_heads())),
                UnaryOp::Parents => Ok(arg.parents()),
                UnaryOp::Children => Ok(arg.children()),
            }
        }
        ExpressionKind::Binary(op, lhs_node, rhs_node) => {
            let lhs = lower_expression(diagnostics, lhs_node, context)?;
            let rhs = lower_expression(diagnostics, rhs_node, context)?;
            match op {
                BinaryOp::Intersection => Ok(lhs.intersection(&rhs)),
                BinaryOp::Difference => Ok(lhs.minus(&rhs)),
                BinaryOp::DagRange => Ok(lhs.dag_range_to(&rhs)),
                BinaryOp::Range => Ok(lhs.range(&rhs)),
            }
        }
        ExpressionKind::UnionAll(nodes) => {
            let expressions: Vec<_> = nodes
                .iter()
                .map(|node| lower_expression(diagnostics, node, context))
                .try_collect()?;
            Ok(RevsetExpression::union_all(&expressions))
        }
        ExpressionKind::FunctionCall(function) => {
            lower_function_call(diagnostics, function, context)
        }
        ExpressionKind::Modifier(modifier) => {
            let name = modifier.name;
            Err(RevsetParseError::expression(
                format!("Modifier `{name}:` is not allowed in sub expression"),
                modifier.name_span,
            ))
        }
        ExpressionKind::AliasExpanded(id, subst) => {
            let mut inner_diagnostics = RevsetDiagnostics::new();
            let expression = lower_expression(&mut inner_diagnostics, subst, context)
                .map_err(|e| e.within_alias_expansion(*id, node.span))?;
            diagnostics.extend_with(inner_diagnostics, |diag| {
                diag.within_alias_expansion(*id, node.span)
            });
            Ok(expression)
        }
    }
}

pub fn parse(
    diagnostics: &mut RevsetDiagnostics,
    revset_str: &str,
    context: &RevsetParseContext,
) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
    let node = parse_program(revset_str)?;
    let node =
        dsl_util::expand_aliases_with_locals(node, context.aliases_map, &context.local_variables)?;
    lower_expression(diagnostics, &node, &context.to_lowering_context())
        .map_err(|err| err.extend_function_candidates(context.aliases_map.function_names()))
}

pub fn parse_with_modifier(
    diagnostics: &mut RevsetDiagnostics,
    revset_str: &str,
    context: &RevsetParseContext,
) -> Result<(Rc<UserRevsetExpression>, Option<RevsetModifier>), RevsetParseError> {
    let node = parse_program(revset_str)?;
    let node =
        dsl_util::expand_aliases_with_locals(node, context.aliases_map, &context.local_variables)?;
    revset_parser::expect_program_with(
        diagnostics,
        &node,
        |diagnostics, node| lower_expression(diagnostics, node, &context.to_lowering_context()),
        |_diagnostics, name, span| match name {
            "all" => Ok(RevsetModifier::All),
            _ => Err(RevsetParseError::with_span(
                RevsetParseErrorKind::NoSuchModifier(name.to_owned()),
                span,
            )),
        },
    )
    .map_err(|err| err.extend_function_candidates(context.aliases_map.function_names()))
}

/// `Some` for rewritten expression, or `None` to reuse the original expression.
type TransformedExpression<St> = Option<Rc<RevsetExpression<St>>>;

/// Walks `expression` tree and applies `f` recursively from leaf nodes.
fn transform_expression_bottom_up<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
    mut f: impl FnMut(&Rc<RevsetExpression<St>>) -> TransformedExpression<St>,
) -> TransformedExpression<St> {
    try_transform_expression::<St, Infallible>(
        expression,
        |_| Ok(None),
        |expression| Ok(f(expression)),
    )
    .unwrap()
}

/// Walks `expression` tree and applies transformation recursively.
///
/// `pre` is the callback to rewrite subtree including children. It is
/// invoked before visiting the child nodes. If returned `Some`, children
/// won't be visited.
///
/// `post` is the callback to rewrite from leaf nodes. If returned `None`,
/// the original expression node will be reused.
///
/// If no nodes rewritten, this function returns `None`.
/// `std::iter::successors()` could be used if the transformation needs to be
/// applied repeatedly until converged.
fn try_transform_expression<St: ExpressionState, E>(
    expression: &Rc<RevsetExpression<St>>,
    mut pre: impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
    mut post: impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
) -> Result<TransformedExpression<St>, E> {
    fn transform_child_rec<St: ExpressionState, E>(
        expression: &Rc<RevsetExpression<St>>,
        pre: &mut impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
        post: &mut impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
    ) -> Result<TransformedExpression<St>, E> {
        Ok(match expression.as_ref() {
            RevsetExpression::None => None,
            RevsetExpression::All => None,
            RevsetExpression::VisibleHeads => None,
            RevsetExpression::Root => None,
            RevsetExpression::Commits(_) => None,
            RevsetExpression::CommitRef(_) => None,
            RevsetExpression::Ancestors { heads, generation } => transform_rec(heads, pre, post)?
                .map(|heads| RevsetExpression::Ancestors {
                    heads,
                    generation: generation.clone(),
                }),
            RevsetExpression::Descendants { roots, generation } => transform_rec(roots, pre, post)?
                .map(|roots| RevsetExpression::Descendants {
                    roots,
                    generation: generation.clone(),
                }),
            RevsetExpression::Range {
                roots,
                heads,
                generation,
            } => transform_rec_pair((roots, heads), pre, post)?.map(|(roots, heads)| {
                RevsetExpression::Range {
                    roots,
                    heads,
                    generation: generation.clone(),
                }
            }),
            RevsetExpression::DagRange { roots, heads } => {
                transform_rec_pair((roots, heads), pre, post)?
                    .map(|(roots, heads)| RevsetExpression::DagRange { roots, heads })
            }
            RevsetExpression::Reachable { sources, domain } => {
                transform_rec_pair((sources, domain), pre, post)?
                    .map(|(sources, domain)| RevsetExpression::Reachable { sources, domain })
            }
            RevsetExpression::Heads(candidates) => {
                transform_rec(candidates, pre, post)?.map(RevsetExpression::Heads)
            }
            RevsetExpression::Roots(candidates) => {
                transform_rec(candidates, pre, post)?.map(RevsetExpression::Roots)
            }
            RevsetExpression::ForkPoint(expression) => {
                transform_rec(expression, pre, post)?.map(RevsetExpression::ForkPoint)
            }
            RevsetExpression::Predecessors(candidates) => {
                transform_rec(candidates, pre, post)?.map(RevsetExpression::Predecessors)
            }
            RevsetExpression::Latest { candidates, count } => transform_rec(candidates, pre, post)?
                .map(|candidates| RevsetExpression::Latest {
                    candidates,
                    count: *count,
                }),
            RevsetExpression::Filter(_) => None,
            RevsetExpression::AsFilter(candidates) => {
                transform_rec(candidates, pre, post)?.map(RevsetExpression::AsFilter)
            }
            RevsetExpression::AtOperation {
                operation,
                candidates,
            } => transform_rec(candidates, pre, post)?.map(|candidates| {
                RevsetExpression::AtOperation {
                    operation: operation.clone(),
                    candidates,
                }
            }),
            RevsetExpression::WithinVisibility {
                candidates,
                visible_heads,
            } => transform_rec(candidates, pre, post)?.map(|candidates| {
                RevsetExpression::WithinVisibility {
                    candidates,
                    visible_heads: visible_heads.clone(),
                }
            }),
            RevsetExpression::Coalesce(expression1, expression2) => transform_rec_pair(
                (expression1, expression2),
                pre,
                post,
            )?
            .map(|(expression1, expression2)| RevsetExpression::Coalesce(expression1, expression2)),
            RevsetExpression::Present(candidates) => {
                transform_rec(candidates, pre, post)?.map(RevsetExpression::Present)
            }
            RevsetExpression::NotIn(complement) => {
                transform_rec(complement, pre, post)?.map(RevsetExpression::NotIn)
            }
            RevsetExpression::Union(expression1, expression2) => {
                transform_rec_pair((expression1, expression2), pre, post)?.map(
                    |(expression1, expression2)| RevsetExpression::Union(expression1, expression2),
                )
            }
            RevsetExpression::Intersection(expression1, expression2) => {
                transform_rec_pair((expression1, expression2), pre, post)?.map(
                    |(expression1, expression2)| {
                        RevsetExpression::Intersection(expression1, expression2)
                    },
                )
            }
            RevsetExpression::Difference(expression1, expression2) => {
                transform_rec_pair((expression1, expression2), pre, post)?.map(
                    |(expression1, expression2)| {
                        RevsetExpression::Difference(expression1, expression2)
                    },
                )
            }
        }
        .map(Rc::new))
    }

    #[expect(clippy::type_complexity)]
    fn transform_rec_pair<St: ExpressionState, E>(
        (expression1, expression2): (&Rc<RevsetExpression<St>>, &Rc<RevsetExpression<St>>),
        pre: &mut impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
        post: &mut impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
    ) -> Result<Option<(Rc<RevsetExpression<St>>, Rc<RevsetExpression<St>>)>, E> {
        match (
            transform_rec(expression1, pre, post)?,
            transform_rec(expression2, pre, post)?,
        ) {
            (Some(new_expression1), Some(new_expression2)) => {
                Ok(Some((new_expression1, new_expression2)))
            }
            (Some(new_expression1), None) => Ok(Some((new_expression1, expression2.clone()))),
            (None, Some(new_expression2)) => Ok(Some((expression1.clone(), new_expression2))),
            (None, None) => Ok(None),
        }
    }

    fn transform_rec<St: ExpressionState, E>(
        expression: &Rc<RevsetExpression<St>>,
        pre: &mut impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
        post: &mut impl FnMut(&Rc<RevsetExpression<St>>) -> Result<TransformedExpression<St>, E>,
    ) -> Result<TransformedExpression<St>, E> {
        if let Some(new_expression) = pre(expression)? {
            return Ok(Some(new_expression));
        }
        if let Some(new_expression) = transform_child_rec(expression, pre, post)? {
            // must propagate new expression tree
            Ok(Some(post(&new_expression)?.unwrap_or(new_expression)))
        } else {
            post(expression)
        }
    }

    transform_rec(expression, &mut pre, &mut post)
}

/// Visitor-like interface to transform [`RevsetExpression`] state recursively.
///
/// This is similar to [`try_transform_expression()`], but is supposed to
/// transform the resolution state from `InSt` to `OutSt`.
trait ExpressionStateFolder<InSt: ExpressionState, OutSt: ExpressionState> {
    type Error;

    /// Transforms the `expression`. By default, inner items are transformed
    /// recursively.
    fn fold_expression(
        &mut self,
        expression: &RevsetExpression<InSt>,
    ) -> Result<Rc<RevsetExpression<OutSt>>, Self::Error> {
        fold_child_expression_state(self, expression)
    }

    /// Transforms commit ref such as symbol.
    fn fold_commit_ref(
        &mut self,
        commit_ref: &InSt::CommitRef,
    ) -> Result<Rc<RevsetExpression<OutSt>>, Self::Error>;

    /// Transforms `at_operation(operation, candidates)` expression.
    fn fold_at_operation(
        &mut self,
        operation: &InSt::Operation,
        candidates: &RevsetExpression<InSt>,
    ) -> Result<Rc<RevsetExpression<OutSt>>, Self::Error>;
}

/// Transforms inner items of the `expression` by using the `folder`.
fn fold_child_expression_state<InSt, OutSt, F>(
    folder: &mut F,
    expression: &RevsetExpression<InSt>,
) -> Result<Rc<RevsetExpression<OutSt>>, F::Error>
where
    InSt: ExpressionState,
    OutSt: ExpressionState,
    F: ExpressionStateFolder<InSt, OutSt> + ?Sized,
{
    let expression: Rc<_> = match expression {
        RevsetExpression::None => RevsetExpression::None.into(),
        RevsetExpression::All => RevsetExpression::All.into(),
        RevsetExpression::VisibleHeads => RevsetExpression::VisibleHeads.into(),
        RevsetExpression::Root => RevsetExpression::Root.into(),
        RevsetExpression::Commits(ids) => RevsetExpression::Commits(ids.clone()).into(),
        RevsetExpression::CommitRef(commit_ref) => folder.fold_commit_ref(commit_ref)?,
        RevsetExpression::Ancestors { heads, generation } => {
            let heads = folder.fold_expression(heads)?;
            let generation = generation.clone();
            RevsetExpression::Ancestors { heads, generation }.into()
        }
        RevsetExpression::Descendants { roots, generation } => {
            let roots = folder.fold_expression(roots)?;
            let generation = generation.clone();
            RevsetExpression::Descendants { roots, generation }.into()
        }
        RevsetExpression::Range {
            roots,
            heads,
            generation,
        } => {
            let roots = folder.fold_expression(roots)?;
            let heads = folder.fold_expression(heads)?;
            let generation = generation.clone();
            RevsetExpression::Range {
                roots,
                heads,
                generation,
            }
            .into()
        }
        RevsetExpression::DagRange { roots, heads } => {
            let roots = folder.fold_expression(roots)?;
            let heads = folder.fold_expression(heads)?;
            RevsetExpression::DagRange { roots, heads }.into()
        }
        RevsetExpression::Reachable { sources, domain } => {
            let sources = folder.fold_expression(sources)?;
            let domain = folder.fold_expression(domain)?;
            RevsetExpression::Reachable { sources, domain }.into()
        }
        RevsetExpression::Heads(heads) => {
            let heads = folder.fold_expression(heads)?;
            RevsetExpression::Heads(heads).into()
        }
        RevsetExpression::Roots(roots) => {
            let roots = folder.fold_expression(roots)?;
            RevsetExpression::Roots(roots).into()
        }
        RevsetExpression::ForkPoint(expression) => {
            let expression = folder.fold_expression(expression)?;
            RevsetExpression::ForkPoint(expression).into()
        }
        RevsetExpression::Predecessors(candidates) => {
            let candidates = folder.fold_expression(candidates)?;
            RevsetExpression::Predecessors(candidates).into()
        }
        RevsetExpression::Latest { candidates, count } => {
            let candidates = folder.fold_expression(candidates)?;
            let count = *count;
            RevsetExpression::Latest { candidates, count }.into()
        }
        RevsetExpression::Filter(predicate) => RevsetExpression::Filter(predicate.clone()).into(),
        RevsetExpression::AsFilter(candidates) => {
            let candidates = folder.fold_expression(candidates)?;
            RevsetExpression::AsFilter(candidates).into()
        }
        RevsetExpression::AtOperation {
            operation,
            candidates,
        } => folder.fold_at_operation(operation, candidates)?,
        RevsetExpression::WithinVisibility {
            candidates,
            visible_heads,
        } => {
            let candidates = folder.fold_expression(candidates)?;
            let visible_heads = visible_heads.clone();
            RevsetExpression::WithinVisibility {
                candidates,
                visible_heads,
            }
            .into()
        }
        RevsetExpression::Coalesce(expression1, expression2) => {
            let expression1 = folder.fold_expression(expression1)?;
            let expression2 = folder.fold_expression(expression2)?;
            RevsetExpression::Coalesce(expression1, expression2).into()
        }
        RevsetExpression::Present(candidates) => {
            let candidates = folder.fold_expression(candidates)?;
            RevsetExpression::Present(candidates).into()
        }
        RevsetExpression::NotIn(complement) => {
            let complement = folder.fold_expression(complement)?;
            RevsetExpression::NotIn(complement).into()
        }
        RevsetExpression::Union(expression1, expression2) => {
            let expression1 = folder.fold_expression(expression1)?;
            let expression2 = folder.fold_expression(expression2)?;
            RevsetExpression::Union(expression1, expression2).into()
        }
        RevsetExpression::Intersection(expression1, expression2) => {
            let expression1 = folder.fold_expression(expression1)?;
            let expression2 = folder.fold_expression(expression2)?;
            RevsetExpression::Intersection(expression1, expression2).into()
        }
        RevsetExpression::Difference(expression1, expression2) => {
            let expression1 = folder.fold_expression(expression1)?;
            let expression2 = folder.fold_expression(expression2)?;
            RevsetExpression::Difference(expression1, expression2).into()
        }
    };
    Ok(expression)
}

/// Transforms filter expressions, by applying the following rules.
///
/// a. Moves as many sets to left of filter intersection as possible, to
///    minimize the filter inputs.
/// b. TODO: Rewrites set operations to and/or/not of predicates, to
///    help further optimization (e.g. combine `file(_)` matchers.)
/// c. Wraps union of filter and set (e.g. `author(_) | heads()`), to
///    ensure inner filter wouldn't need to evaluate all the input sets.
fn internalize_filter<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
) -> TransformedExpression<St> {
    fn is_filter<St: ExpressionState>(expression: &RevsetExpression<St>) -> bool {
        matches!(
            expression,
            RevsetExpression::Filter(_) | RevsetExpression::AsFilter(_)
        )
    }

    fn is_filter_tree<St: ExpressionState>(expression: &RevsetExpression<St>) -> bool {
        is_filter(expression) || as_filter_intersection(expression).is_some()
    }

    // Extracts 'c & f' from intersect_down()-ed node.
    #[expect(clippy::type_complexity)]
    fn as_filter_intersection<St: ExpressionState>(
        expression: &RevsetExpression<St>,
    ) -> Option<(&Rc<RevsetExpression<St>>, &Rc<RevsetExpression<St>>)> {
        if let RevsetExpression::Intersection(expression1, expression2) = expression {
            is_filter(expression2).then_some((expression1, expression2))
        } else {
            None
        }
    }

    // Since both sides must have already been intersect_down()-ed, we don't need to
    // apply the whole bottom-up pass to new intersection node. Instead, just push
    // new 'c & (d & g)' down-left to '(c & d) & g' while either side is
    // an intersection of filter node.
    fn intersect_down<St: ExpressionState>(
        expression1: &Rc<RevsetExpression<St>>,
        expression2: &Rc<RevsetExpression<St>>,
    ) -> TransformedExpression<St> {
        let recurse = |e1, e2| intersect_down(e1, e2).unwrap_or_else(|| e1.intersection(e2));
        match (expression1.as_ref(), expression2.as_ref()) {
            // Don't reorder 'f1 & f2'
            (_, e2) if is_filter(e2) => None,
            // f1 & e2 -> e2 & f1
            (e1, _) if is_filter(e1) => Some(expression2.intersection(expression1)),
            (e1, e2) => match (as_filter_intersection(e1), as_filter_intersection(e2)) {
                // e1 & (c2 & f2) -> (e1 & c2) & f2
                // (c1 & f1) & (c2 & f2) -> ((c1 & f1) & c2) & f2 -> ((c1 & c2) & f1) & f2
                (_, Some((c2, f2))) => Some(recurse(expression1, c2).intersection(f2)),
                // (c1 & f1) & e2 -> (c1 & e2) & f1
                // ((c1 & f1) & g1) & e2 -> ((c1 & f1) & e2) & g1 -> ((c1 & e2) & f1) & g1
                (Some((c1, f1)), _) => Some(recurse(c1, expression2).intersection(f1)),
                (None, None) => None,
            },
        }
    }

    // Bottom-up pass pulls up-right filter node from leaf '(c & f) & e' ->
    // '(c & e) & f', so that an intersection of filter node can be found as
    // a direct child of another intersection node. However, the rewritten
    // intersection node 'c & e' can also be a rewrite target if 'e' contains
    // a filter node. That's why intersect_down() is also recursive.
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::Present(e) => {
            is_filter_tree(e).then(|| Rc::new(RevsetExpression::AsFilter(expression.clone())))
        }
        RevsetExpression::NotIn(e) => {
            is_filter_tree(e).then(|| Rc::new(RevsetExpression::AsFilter(expression.clone())))
        }
        RevsetExpression::Union(e1, e2) => (is_filter_tree(e1) || is_filter_tree(e2))
            .then(|| Rc::new(RevsetExpression::AsFilter(expression.clone()))),
        RevsetExpression::Intersection(expression1, expression2) => {
            intersect_down(expression1, expression2)
        }
        // Difference(e1, e2) should have been unfolded to Intersection(e1, NotIn(e2)).
        _ => None,
    })
}

/// Eliminates redundant nodes like `x & all()`, `~~x`.
///
/// This does not rewrite 'x & none()' to 'none()' because 'x' may be an invalid
/// symbol.
fn fold_redundant_expression<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
) -> TransformedExpression<St> {
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::NotIn(outer) => match outer.as_ref() {
            RevsetExpression::NotIn(inner) => Some(inner.clone()),
            _ => None,
        },
        RevsetExpression::Intersection(expression1, expression2) => {
            match (expression1.as_ref(), expression2.as_ref()) {
                (_, RevsetExpression::All) => Some(expression1.clone()),
                (RevsetExpression::All, _) => Some(expression2.clone()),
                _ => None,
            }
        }
        _ => None,
    })
}

fn to_difference_range<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
    complement: &Rc<RevsetExpression<St>>,
) -> TransformedExpression<St> {
    match (expression.as_ref(), complement.as_ref()) {
        // ::heads & ~(::roots) -> roots..heads
        (
            RevsetExpression::Ancestors { heads, generation },
            RevsetExpression::Ancestors {
                heads: roots,
                generation: GENERATION_RANGE_FULL,
            },
        ) => Some(Rc::new(RevsetExpression::Range {
            roots: roots.clone(),
            heads: heads.clone(),
            generation: generation.clone(),
        })),
        // ::heads & ~(::roots-) -> ::heads & ~ancestors(roots, 1..) -> roots-..heads
        (
            RevsetExpression::Ancestors { heads, generation },
            RevsetExpression::Ancestors {
                heads: roots,
                generation:
                    Range {
                        start: roots_start,
                        end: u64::MAX,
                    },
            },
        ) => Some(Rc::new(RevsetExpression::Range {
            roots: roots.ancestors_at(*roots_start),
            heads: heads.clone(),
            generation: generation.clone(),
        })),
        _ => None,
    }
}

/// Transforms negative intersection to difference. Redundant intersections like
/// `all() & e` should have been removed.
fn fold_difference<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
) -> TransformedExpression<St> {
    fn to_difference<St: ExpressionState>(
        expression: &Rc<RevsetExpression<St>>,
        complement: &Rc<RevsetExpression<St>>,
    ) -> Rc<RevsetExpression<St>> {
        to_difference_range(expression, complement).unwrap_or_else(|| expression.minus(complement))
    }

    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::Intersection(expression1, expression2) => {
            match (expression1.as_ref(), expression2.as_ref()) {
                // For '~x & f', don't move filter node 'f' left
                (_, RevsetExpression::Filter(_) | RevsetExpression::AsFilter(_)) => None,
                (_, RevsetExpression::NotIn(complement)) => {
                    Some(to_difference(expression1, complement))
                }
                (RevsetExpression::NotIn(complement), _) => {
                    Some(to_difference(expression2, complement))
                }
                _ => None,
            }
        }
        _ => None,
    })
}

/// Transforms remaining negated ancestors `~(::h)` to range `h..`.
///
/// Since this rule inserts redundant `visible_heads()`, negative intersections
/// should have been transformed.
fn fold_not_in_ancestors<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
) -> TransformedExpression<St> {
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::NotIn(complement)
            if matches!(complement.as_ref(), RevsetExpression::Ancestors { .. }) =>
        {
            // ~(::heads) -> heads..
            // ~(::heads-) -> ~ancestors(heads, 1..) -> heads-..
            to_difference_range(&RevsetExpression::visible_heads().ancestors(), complement)
        }
        _ => None,
    })
}

/// Transforms binary difference to more primitive negative intersection.
///
/// For example, `all() ~ e` will become `all() & ~e`, which can be simplified
/// further by `fold_redundant_expression()`.
fn unfold_difference<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
) -> TransformedExpression<St> {
    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        // roots..heads -> ::heads & ~(::roots)
        RevsetExpression::Range {
            roots,
            heads,
            generation,
        } => {
            let heads_ancestors = Rc::new(RevsetExpression::Ancestors {
                heads: heads.clone(),
                generation: generation.clone(),
            });
            Some(heads_ancestors.intersection(&roots.ancestors().negated()))
        }
        RevsetExpression::Difference(expression1, expression2) => {
            Some(expression1.intersection(&expression2.negated()))
        }
        _ => None,
    })
}

/// Transforms nested `ancestors()`/`parents()`/`descendants()`/`children()`
/// like `h---`/`r+++`.
fn fold_generation<St: ExpressionState>(
    expression: &Rc<RevsetExpression<St>>,
) -> TransformedExpression<St> {
    fn add_generation(generation1: &Range<u64>, generation2: &Range<u64>) -> Range<u64> {
        // For any (g1, g2) in (generation1, generation2), g1 + g2.
        if generation1.is_empty() || generation2.is_empty() {
            GENERATION_RANGE_EMPTY
        } else {
            let start = u64::saturating_add(generation1.start, generation2.start);
            let end = u64::saturating_add(generation1.end, generation2.end - 1);
            start..end
        }
    }

    transform_expression_bottom_up(expression, |expression| match expression.as_ref() {
        RevsetExpression::Ancestors {
            heads,
            generation: generation1,
        } => {
            match heads.as_ref() {
                // (h-)- -> ancestors(ancestors(h, 1), 1) -> ancestors(h, 2)
                // ::(h-) -> ancestors(ancestors(h, 1), ..) -> ancestors(h, 1..)
                // (::h)- -> ancestors(ancestors(h, ..), 1) -> ancestors(h, 1..)
                RevsetExpression::Ancestors {
                    heads,
                    generation: generation2,
                } => Some(Rc::new(RevsetExpression::Ancestors {
                    heads: heads.clone(),
                    generation: add_generation(generation1, generation2),
                })),
                _ => None,
            }
        }
        RevsetExpression::Descendants {
            roots,
            generation: generation1,
        } => {
            match roots.as_ref() {
                // (r+)+ -> descendants(descendants(r, 1), 1) -> descendants(r, 2)
                // (r+):: -> descendants(descendants(r, 1), ..) -> descendants(r, 1..)
                // (r::)+ -> descendants(descendants(r, ..), 1) -> descendants(r, 1..)
                RevsetExpression::Descendants {
                    roots,
                    generation: generation2,
                } => Some(Rc::new(RevsetExpression::Descendants {
                    roots: roots.clone(),
                    generation: add_generation(generation1, generation2),
                })),
                _ => None,
            }
        }
        // Range should have been unfolded to intersection of Ancestors.
        _ => None,
    })
}

/// Rewrites the given `expression` tree to reduce evaluation cost. Returns new
/// tree.
pub fn optimize<St: ExpressionState>(
    expression: Rc<RevsetExpression<St>>,
) -> Rc<RevsetExpression<St>> {
    let expression = unfold_difference(&expression).unwrap_or(expression);
    let expression = fold_redundant_expression(&expression).unwrap_or(expression);
    let expression = fold_generation(&expression).unwrap_or(expression);
    let expression = internalize_filter(&expression).unwrap_or(expression);
    let expression = fold_difference(&expression).unwrap_or(expression);
    fold_not_in_ancestors(&expression).unwrap_or(expression)
}

// TODO: find better place to host this function (or add compile-time revset
// parsing and resolution like
// `revset!("{unwanted}..{wanted}").evaluate(repo)`?)
pub fn walk_revs<'index>(
    repo: &'index dyn Repo,
    wanted: &[CommitId],
    unwanted: &[CommitId],
) -> Result<Box<dyn Revset + 'index>, RevsetEvaluationError> {
    RevsetExpression::commits(unwanted.to_vec())
        .range(&RevsetExpression::commits(wanted.to_vec()))
        .evaluate(repo)
}

fn reload_repo_at_operation(
    repo: &dyn Repo,
    op_str: &str,
) -> Result<Arc<ReadonlyRepo>, RevsetResolutionError> {
    // TODO: Maybe we should ensure that the resolved operation is an ancestor
    // of the current operation. If it weren't, there might be commits unknown
    // to the outer repo.
    let base_repo = repo.base_repo();
    let operation = op_walk::resolve_op_with_repo(base_repo, op_str)
        .map_err(|err| RevsetResolutionError::Other(err.into()))?;
    base_repo.reload_at(&operation).map_err(|err| match err {
        RepoLoaderError::Backend(err) => RevsetResolutionError::Backend(err),
        RepoLoaderError::IndexRead(_)
        | RepoLoaderError::OpHeadResolution(_)
        | RepoLoaderError::OpHeadsStoreError(_)
        | RepoLoaderError::OpStore(_)
        | RepoLoaderError::TransactionCommit(_) => RevsetResolutionError::Other(err.into()),
    })
}

fn resolve_remote_bookmark(repo: &dyn Repo, symbol: RemoteRefSymbol<'_>) -> Option<Vec<CommitId>> {
    let target = &repo.view().get_remote_bookmark(symbol).target;
    target
        .is_present()
        .then(|| target.added_ids().cloned().collect())
}

fn all_formatted_bookmark_symbols(
    repo: &dyn Repo,
    include_synced_remotes: bool,
) -> impl Iterator<Item = String> + use<'_> {
    let view = repo.view();
    view.bookmarks().flat_map(move |(name, bookmark_target)| {
        let local_target = bookmark_target.local_target;
        let local_symbol = local_target
            .is_present()
            .then(|| format_symbol(name.as_str()));
        let remote_symbols = bookmark_target
            .remote_refs
            .into_iter()
            .filter(move |&(_, remote_ref)| {
                include_synced_remotes
                    || !remote_ref.is_tracked()
                    || remote_ref.target != *local_target
            })
            .map(move |(remote, _)| format_remote_symbol(name.as_str(), remote.as_str()));
        local_symbol.into_iter().chain(remote_symbols)
    })
}

fn make_no_such_symbol_error(repo: &dyn Repo, name: String) -> RevsetResolutionError {
    // TODO: include tags?
    let bookmark_names = all_formatted_bookmark_symbols(repo, name.contains('@'));
    let candidates = collect_similar(&name, bookmark_names);
    RevsetResolutionError::NoSuchRevision { name, candidates }
}

pub trait SymbolResolver {
    /// Looks up `symbol` in the given `repo`.
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Vec<CommitId>, RevsetResolutionError>;
}

/// Fails on any attempt to resolve a symbol.
pub struct FailingSymbolResolver;

impl SymbolResolver for FailingSymbolResolver {
    fn resolve_symbol(
        &self,
        _repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Vec<CommitId>, RevsetResolutionError> {
        Err(RevsetResolutionError::NoSuchRevision {
            name: format!(
                "Won't resolve symbol {symbol:?}. When creating revsets programmatically, avoid \
                 using RevsetExpression::symbol(); use RevsetExpression::commits() instead."
            ),
            candidates: Default::default(),
        })
    }
}

/// A symbol resolver for a specific namespace of labels.
///
/// Returns None if it cannot handle the symbol.
pub trait PartialSymbolResolver {
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Option<Vec<CommitId>>, RevsetResolutionError>;
}

struct TagResolver;

impl PartialSymbolResolver for TagResolver {
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Option<Vec<CommitId>>, RevsetResolutionError> {
        let target = repo.view().get_tag(symbol.as_ref());
        Ok(target
            .is_present()
            .then(|| target.added_ids().cloned().collect()))
    }
}

struct BookmarkResolver;

impl PartialSymbolResolver for BookmarkResolver {
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Option<Vec<CommitId>>, RevsetResolutionError> {
        let target = repo.view().get_local_bookmark(symbol.as_ref());
        Ok(target
            .is_present()
            .then(|| target.added_ids().cloned().collect()))
    }
}

struct GitRefResolver;

impl PartialSymbolResolver for GitRefResolver {
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Option<Vec<CommitId>>, RevsetResolutionError> {
        let view = repo.view();
        for git_ref_prefix in &["", "refs/"] {
            let target = view.get_git_ref([git_ref_prefix, symbol].concat().as_ref());
            if target.is_present() {
                return Ok(Some(target.added_ids().cloned().collect()));
            }
        }

        Ok(None)
    }
}

const DEFAULT_RESOLVERS: &[&'static dyn PartialSymbolResolver] =
    &[&TagResolver, &BookmarkResolver, &GitRefResolver];

struct CommitPrefixResolver<'a> {
    context_repo: &'a dyn Repo,
    context: Option<&'a IdPrefixContext>,
}

impl PartialSymbolResolver for CommitPrefixResolver<'_> {
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Option<Vec<CommitId>>, RevsetResolutionError> {
        if let Some(prefix) = HexPrefix::new(symbol) {
            let index = self
                .context
                .map(|ctx| ctx.populate(self.context_repo))
                .transpose()
                .map_err(|err| RevsetResolutionError::Other(err.into()))?
                .unwrap_or(IdPrefixIndex::empty());
            match index.resolve_commit_prefix(repo, &prefix) {
                PrefixResolution::AmbiguousMatch => Err(
                    RevsetResolutionError::AmbiguousCommitIdPrefix(symbol.to_owned()),
                ),
                PrefixResolution::SingleMatch(id) => Ok(Some(vec![id])),
                PrefixResolution::NoMatch => Ok(None),
            }
        } else {
            Ok(None)
        }
    }
}

struct ChangePrefixResolver<'a> {
    context_repo: &'a dyn Repo,
    context: Option<&'a IdPrefixContext>,
}

impl PartialSymbolResolver for ChangePrefixResolver<'_> {
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Option<Vec<CommitId>>, RevsetResolutionError> {
        if let Some(prefix) = to_forward_hex(symbol).as_deref().and_then(HexPrefix::new) {
            let index = self
                .context
                .map(|ctx| ctx.populate(self.context_repo))
                .transpose()
                .map_err(|err| RevsetResolutionError::Other(err.into()))?
                .unwrap_or(IdPrefixIndex::empty());
            match index.resolve_change_prefix(repo, &prefix) {
                PrefixResolution::AmbiguousMatch => Err(
                    RevsetResolutionError::AmbiguousChangeIdPrefix(symbol.to_owned()),
                ),
                PrefixResolution::SingleMatch(ids) => Ok(Some(ids)),
                PrefixResolution::NoMatch => Ok(None),
            }
        } else {
            Ok(None)
        }
    }
}

/// An extension of the DefaultSymbolResolver.
///
/// Each PartialSymbolResolver will be invoked in order, its result used if one
/// is provided. Native resolvers are always invoked first. In the future, we
/// may provide a way for extensions to override native resolvers like tags and
/// bookmarks.
pub trait SymbolResolverExtension {
    /// PartialSymbolResolvers can initialize some global data by using the
    /// `context_repo`, but the `context_repo` may point to a different
    /// operation from the `repo` passed into `resolve_symbol()`. For
    /// resolution, the latter `repo` should be used.
    fn new_resolvers<'a>(
        &self,
        context_repo: &'a dyn Repo,
    ) -> Vec<Box<dyn PartialSymbolResolver + 'a>>;
}

/// Resolves bookmarks, remote bookmarks, tags, git refs, and full and
/// abbreviated commit and change ids.
pub struct DefaultSymbolResolver<'a> {
    commit_id_resolver: CommitPrefixResolver<'a>,
    change_id_resolver: ChangePrefixResolver<'a>,
    extensions: Vec<Box<dyn PartialSymbolResolver + 'a>>,
}

impl<'a> DefaultSymbolResolver<'a> {
    /// Creates new symbol resolver that will first disambiguate short ID
    /// prefixes within the given `context_repo` if configured.
    pub fn new(
        context_repo: &'a dyn Repo,
        extensions: &[impl AsRef<dyn SymbolResolverExtension>],
    ) -> Self {
        DefaultSymbolResolver {
            commit_id_resolver: CommitPrefixResolver {
                context_repo,
                context: None,
            },
            change_id_resolver: ChangePrefixResolver {
                context_repo,
                context: None,
            },
            extensions: extensions
                .iter()
                .flat_map(|ext| ext.as_ref().new_resolvers(context_repo))
                .collect(),
        }
    }

    pub fn with_id_prefix_context(mut self, id_prefix_context: &'a IdPrefixContext) -> Self {
        self.commit_id_resolver.context = Some(id_prefix_context);
        self.change_id_resolver.context = Some(id_prefix_context);
        self
    }

    fn partial_resolvers(&self) -> impl Iterator<Item = &(dyn PartialSymbolResolver + 'a)> {
        let prefix_resolvers: [&dyn PartialSymbolResolver; 2] =
            [&self.commit_id_resolver, &self.change_id_resolver];
        itertools::chain!(
            DEFAULT_RESOLVERS.iter().copied(),
            prefix_resolvers,
            self.extensions.iter().map(|e| e.as_ref())
        )
    }
}

impl SymbolResolver for DefaultSymbolResolver<'_> {
    fn resolve_symbol(
        &self,
        repo: &dyn Repo,
        symbol: &str,
    ) -> Result<Vec<CommitId>, RevsetResolutionError> {
        if symbol.is_empty() {
            return Err(RevsetResolutionError::EmptyString);
        }

        for partial_resolver in self.partial_resolvers() {
            if let Some(ids) = partial_resolver.resolve_symbol(repo, symbol)? {
                return Ok(ids);
            }
        }

        Err(make_no_such_symbol_error(repo, format_symbol(symbol)))
    }
}

fn resolve_commit_ref(
    repo: &dyn Repo,
    commit_ref: &RevsetCommitRef,
    symbol_resolver: &dyn SymbolResolver,
) -> Result<Vec<CommitId>, RevsetResolutionError> {
    match commit_ref {
        RevsetCommitRef::Symbol(symbol) => symbol_resolver.resolve_symbol(repo, symbol),
        RevsetCommitRef::RemoteSymbol(symbol) => resolve_remote_bookmark(repo, symbol.as_ref())
            .ok_or_else(|| make_no_such_symbol_error(repo, symbol.to_string())),
        RevsetCommitRef::WorkingCopy(name) => {
            if let Some(commit_id) = repo.view().get_wc_commit_id(name) {
                Ok(vec![commit_id.clone()])
            } else {
                Err(RevsetResolutionError::WorkspaceMissingWorkingCopy { name: name.clone() })
            }
        }
        RevsetCommitRef::WorkingCopies => {
            let wc_commits = repo.view().wc_commit_ids().values().cloned().collect_vec();
            Ok(wc_commits)
        }
        RevsetCommitRef::Bookmarks(pattern) => {
            let commit_ids = repo
                .view()
                .local_bookmarks_matching(pattern)
                .flat_map(|(_, target)| target.added_ids())
                .cloned()
                .collect();
            Ok(commit_ids)
        }
        RevsetCommitRef::RemoteBookmarks {
            bookmark_pattern,
            remote_pattern,
            remote_ref_state,
        } => {
            // TODO: should we allow to select @git bookmarks explicitly?
            let commit_ids = repo
                .view()
                .remote_bookmarks_matching(bookmark_pattern, remote_pattern)
                .filter(|(_, remote_ref)| {
                    remote_ref_state.is_none_or(|state| remote_ref.state == state)
                })
                .filter(|&(symbol, _)| !crate::git::is_special_git_remote(symbol.remote))
                .flat_map(|(_, remote_ref)| remote_ref.target.added_ids())
                .cloned()
                .collect();
            Ok(commit_ids)
        }
        RevsetCommitRef::Tags(pattern) => {
            let commit_ids = repo
                .view()
                .tags_matching(pattern)
                .flat_map(|(_, target)| target.added_ids())
                .cloned()
                .collect();
            Ok(commit_ids)
        }
        RevsetCommitRef::GitRefs => {
            let mut commit_ids = vec![];
            for ref_target in repo.view().git_refs().values() {
                commit_ids.extend(ref_target.added_ids().cloned());
            }
            Ok(commit_ids)
        }
        RevsetCommitRef::GitHead => Ok(repo.view().git_head().added_ids().cloned().collect()),
    }
}

/// Resolves symbols and commit refs recursively.
struct ExpressionSymbolResolver<'a> {
    base_repo: &'a dyn Repo,
    repo_stack: Vec<Arc<ReadonlyRepo>>,
    symbol_resolver: &'a dyn SymbolResolver,
}

impl<'a> ExpressionSymbolResolver<'a> {
    fn new(base_repo: &'a dyn Repo, symbol_resolver: &'a dyn SymbolResolver) -> Self {
        ExpressionSymbolResolver {
            base_repo,
            repo_stack: vec![],
            symbol_resolver,
        }
    }

    fn repo(&self) -> &dyn Repo {
        self.repo_stack
            .last()
            .map_or(self.base_repo, |repo| repo.as_ref())
    }
}

impl ExpressionStateFolder<UserExpressionState, ResolvedExpressionState>
    for ExpressionSymbolResolver<'_>
{
    type Error = RevsetResolutionError;

    fn fold_expression(
        &mut self,
        expression: &UserRevsetExpression,
    ) -> Result<Rc<ResolvedRevsetExpression>, Self::Error> {
        match expression {
            // 'present(x)' opens new symbol resolution scope to map error to 'none()'
            RevsetExpression::Present(candidates) => {
                self.fold_expression(candidates).or_else(|err| match err {
                    RevsetResolutionError::NoSuchRevision { .. }
                    | RevsetResolutionError::WorkspaceMissingWorkingCopy { .. } => {
                        Ok(RevsetExpression::none())
                    }
                    RevsetResolutionError::EmptyString
                    | RevsetResolutionError::AmbiguousCommitIdPrefix(_)
                    | RevsetResolutionError::AmbiguousChangeIdPrefix(_)
                    | RevsetResolutionError::Backend(_)
                    | RevsetResolutionError::Other(_) => Err(err),
                })
            }
            _ => fold_child_expression_state(self, expression),
        }
    }

    fn fold_commit_ref(
        &mut self,
        commit_ref: &RevsetCommitRef,
    ) -> Result<Rc<ResolvedRevsetExpression>, Self::Error> {
        let commit_ids = resolve_commit_ref(self.repo(), commit_ref, self.symbol_resolver)?;
        Ok(RevsetExpression::commits(commit_ids))
    }

    fn fold_at_operation(
        &mut self,
        operation: &String,
        candidates: &UserRevsetExpression,
    ) -> Result<Rc<ResolvedRevsetExpression>, Self::Error> {
        let repo = reload_repo_at_operation(self.repo(), operation)?;
        self.repo_stack.push(repo);
        let candidates = self.fold_expression(candidates)?;
        let visible_heads = self.repo().view().heads().iter().cloned().collect();
        self.repo_stack.pop();
        Ok(Rc::new(RevsetExpression::WithinVisibility {
            candidates,
            visible_heads,
        }))
    }
}

fn resolve_symbols(
    repo: &dyn Repo,
    expression: &UserRevsetExpression,
    symbol_resolver: &dyn SymbolResolver,
) -> Result<Rc<ResolvedRevsetExpression>, RevsetResolutionError> {
    let mut resolver = ExpressionSymbolResolver::new(repo, symbol_resolver);
    resolver.fold_expression(expression)
}

/// Inserts implicit `all()` and `visible_heads()` nodes to the `expression`.
///
/// Symbols and commit refs in the `expression` should have been resolved.
///
/// This is a separate step because a symbol-resolved `expression` could be
/// transformed further to e.g. combine OR-ed `Commits(_)`, or to collect
/// commit ids to make `all()` include hidden-but-specified commits. The
/// return type `ResolvedExpression` is stricter than `RevsetExpression`,
/// and isn't designed for such transformation.
fn resolve_visibility(
    repo: &dyn Repo,
    expression: &ResolvedRevsetExpression,
) -> ResolvedExpression {
    let context = VisibilityResolutionContext {
        visible_heads: &repo.view().heads().iter().cloned().collect_vec(),
        root: repo.store().root_commit_id(),
    };
    context.resolve(expression)
}

#[derive(Clone, Debug)]
struct VisibilityResolutionContext<'a> {
    visible_heads: &'a [CommitId],
    root: &'a CommitId,
}

impl VisibilityResolutionContext<'_> {
    /// Resolves expression tree as set.
    fn resolve(&self, expression: &ResolvedRevsetExpression) -> ResolvedExpression {
        match expression {
            RevsetExpression::None => ResolvedExpression::Commits(vec![]),
            RevsetExpression::All => self.resolve_all(),
            RevsetExpression::VisibleHeads => self.resolve_visible_heads(),
            RevsetExpression::Root => self.resolve_root(),
            RevsetExpression::Commits(commit_ids) => {
                ResolvedExpression::Commits(commit_ids.clone())
            }
            RevsetExpression::CommitRef(commit_ref) => match *commit_ref {},
            RevsetExpression::Ancestors { heads, generation } => ResolvedExpression::Ancestors {
                heads: self.resolve(heads).into(),
                generation: generation.clone(),
            },
            RevsetExpression::Descendants { roots, generation } => ResolvedExpression::DagRange {
                roots: self.resolve(roots).into(),
                heads: self.resolve_visible_heads().into(),
                generation_from_roots: generation.clone(),
            },
            RevsetExpression::Range {
                roots,
                heads,
                generation,
            } => ResolvedExpression::Range {
                roots: self.resolve(roots).into(),
                heads: self.resolve(heads).into(),
                generation: generation.clone(),
            },
            RevsetExpression::DagRange { roots, heads } => ResolvedExpression::DagRange {
                roots: self.resolve(roots).into(),
                heads: self.resolve(heads).into(),
                generation_from_roots: GENERATION_RANGE_FULL,
            },
            RevsetExpression::Reachable { sources, domain } => ResolvedExpression::Reachable {
                sources: self.resolve(sources).into(),
                domain: self.resolve(domain).into(),
            },
            RevsetExpression::Heads(candidates) => {
                ResolvedExpression::Heads(self.resolve(candidates).into())
            }
            RevsetExpression::Roots(candidates) => {
                ResolvedExpression::Roots(self.resolve(candidates).into())
            }
            RevsetExpression::ForkPoint(expression) => {
                ResolvedExpression::ForkPoint(self.resolve(expression).into())
            }
            RevsetExpression::Predecessors(candidates) => {
                ResolvedExpression::Predecessors(self.resolve(candidates).into())
            }
            RevsetExpression::Latest { candidates, count } => ResolvedExpression::Latest {
                candidates: self.resolve(candidates).into(),
                count: *count,
            },
            RevsetExpression::Filter(_) | RevsetExpression::AsFilter(_) => {
                // Top-level filter without intersection: e.g. "~author(_)" is represented as
                // `AsFilter(NotIn(Filter(Author(_))))`.
                ResolvedExpression::FilterWithin {
                    candidates: self.resolve_all().into(),
                    predicate: self.resolve_predicate(expression),
                }
            }
            RevsetExpression::AtOperation { operation, .. } => match *operation {},
            RevsetExpression::WithinVisibility {
                candidates,
                visible_heads,
            } => {
                let context = VisibilityResolutionContext {
                    visible_heads,
                    root: self.root,
                };
                context.resolve(candidates)
            }
            RevsetExpression::Coalesce(expression1, expression2) => ResolvedExpression::Coalesce(
                self.resolve(expression1).into(),
                self.resolve(expression2).into(),
            ),
            // present(x) is noop if x doesn't contain any commit refs.
            RevsetExpression::Present(candidates) => self.resolve(candidates),
            RevsetExpression::NotIn(complement) => ResolvedExpression::Difference(
                self.resolve_all().into(),
                self.resolve(complement).into(),
            ),
            RevsetExpression::Union(expression1, expression2) => ResolvedExpression::Union(
                self.resolve(expression1).into(),
                self.resolve(expression2).into(),
            ),
            RevsetExpression::Intersection(expression1, expression2) => {
                match expression2.as_ref() {
                    RevsetExpression::Filter(_) | RevsetExpression::AsFilter(_) => {
                        ResolvedExpression::FilterWithin {
                            candidates: self.resolve(expression1).into(),
                            predicate: self.resolve_predicate(expression2),
                        }
                    }
                    _ => ResolvedExpression::Intersection(
                        self.resolve(expression1).into(),
                        self.resolve(expression2).into(),
                    ),
                }
            }
            RevsetExpression::Difference(expression1, expression2) => {
                ResolvedExpression::Difference(
                    self.resolve(expression1).into(),
                    self.resolve(expression2).into(),
                )
            }
        }
    }

    fn resolve_all(&self) -> ResolvedExpression {
        // Since `all()` does not include hidden commits, some of the logical
        // transformation rules may subtly change the evaluated set. For example,
        // `all() & x` is not `x` if `x` is hidden. This wouldn't matter in practice,
        // but if it does, the heads set could be extended to include the commits
        // (and `remote_bookmarks()`) specified in the revset expression. Alternatively,
        // some optimization rules could be removed, but that means `author(_) & x`
        // would have to test `::visible_heads() & x`.
        ResolvedExpression::Ancestors {
            heads: self.resolve_visible_heads().into(),
            generation: GENERATION_RANGE_FULL,
        }
    }

    fn resolve_visible_heads(&self) -> ResolvedExpression {
        ResolvedExpression::Commits(self.visible_heads.to_owned())
    }

    fn resolve_root(&self) -> ResolvedExpression {
        ResolvedExpression::Commits(vec![self.root.to_owned()])
    }

    /// Resolves expression tree as filter predicate.
    ///
    /// For filter expression, this never inserts a hidden `all()` since a
    /// filter predicate doesn't need to produce revisions to walk.
    fn resolve_predicate(
        &self,
        expression: &ResolvedRevsetExpression,
    ) -> ResolvedPredicateExpression {
        match expression {
            RevsetExpression::None
            | RevsetExpression::All
            | RevsetExpression::VisibleHeads
            | RevsetExpression::Root
            | RevsetExpression::Commits(_)
            | RevsetExpression::CommitRef(_)
            | RevsetExpression::Ancestors { .. }
            | RevsetExpression::Descendants { .. }
            | RevsetExpression::Range { .. }
            | RevsetExpression::DagRange { .. }
            | RevsetExpression::Reachable { .. }
            | RevsetExpression::Heads(_)
            | RevsetExpression::Roots(_)
            | RevsetExpression::ForkPoint(_)
            | RevsetExpression::Predecessors(_)
            | RevsetExpression::Latest { .. } => {
                ResolvedPredicateExpression::Set(self.resolve(expression).into())
            }
            RevsetExpression::Filter(predicate) => {
                ResolvedPredicateExpression::Filter(predicate.clone())
            }
            RevsetExpression::AsFilter(candidates) => self.resolve_predicate(candidates),
            RevsetExpression::AtOperation { operation, .. } => match *operation {},
            // Filters should be intersected with all() within the at-op repo.
            RevsetExpression::WithinVisibility { .. } => {
                ResolvedPredicateExpression::Set(self.resolve(expression).into())
            }
            RevsetExpression::Coalesce(_, _) => {
                ResolvedPredicateExpression::Set(self.resolve(expression).into())
            }
            // present(x) is noop if x doesn't contain any commit refs.
            RevsetExpression::Present(candidates) => self.resolve_predicate(candidates),
            RevsetExpression::NotIn(complement) => {
                ResolvedPredicateExpression::NotIn(self.resolve_predicate(complement).into())
            }
            RevsetExpression::Union(expression1, expression2) => {
                let predicate1 = self.resolve_predicate(expression1);
                let predicate2 = self.resolve_predicate(expression2);
                ResolvedPredicateExpression::Union(predicate1.into(), predicate2.into())
            }
            // Intersection of filters should have been substituted by optimize().
            // If it weren't, just fall back to the set evaluation path.
            RevsetExpression::Intersection(..) | RevsetExpression::Difference(..) => {
                ResolvedPredicateExpression::Set(self.resolve(expression).into())
            }
        }
    }
}

pub trait Revset: fmt::Debug {
    /// Iterate in topological order with children before parents.
    fn iter<'a>(&self) -> Box<dyn Iterator<Item = Result<CommitId, RevsetEvaluationError>> + 'a>
    where
        Self: 'a;

    /// Iterates commit/change id pairs in topological order.
    fn commit_change_ids<'a>(
        &self,
    ) -> Box<dyn Iterator<Item = Result<(CommitId, ChangeId), RevsetEvaluationError>> + 'a>
    where
        Self: 'a;

    fn iter_graph<'a>(
        &self,
    ) -> Box<dyn Iterator<Item = Result<GraphNode<CommitId>, RevsetEvaluationError>> + 'a>
    where
        Self: 'a;

    /// Returns true if iterator will emit no commit nor error.
    fn is_empty(&self) -> bool;

    /// Inclusive lower bound and, optionally, inclusive upper bound of how many
    /// commits are in the revset. The implementation can use its discretion as
    /// to how much effort should be put into the estimation, and how accurate
    /// the resulting estimate should be.
    fn count_estimate(&self) -> Result<(usize, Option<usize>), RevsetEvaluationError>;

    /// Returns a closure that checks if a commit is contained within the
    /// revset.
    ///
    /// The implementation may construct and maintain any necessary internal
    /// context to optimize the performance of the check.
    fn containing_fn<'a>(&self) -> Box<RevsetContainingFn<'a>>
    where
        Self: 'a;
}

/// Function that checks if a commit is contained within the revset.
pub type RevsetContainingFn<'a> = dyn Fn(&CommitId) -> Result<bool, RevsetEvaluationError> + 'a;

pub trait RevsetIteratorExt<I> {
    fn commits(self, store: &Arc<Store>) -> RevsetCommitIterator<I>;
}

impl<I: Iterator<Item = Result<CommitId, RevsetEvaluationError>>> RevsetIteratorExt<I> for I {
    fn commits(self, store: &Arc<Store>) -> RevsetCommitIterator<I> {
        RevsetCommitIterator {
            iter: self,
            store: store.clone(),
        }
    }
}

pub struct RevsetCommitIterator<I> {
    store: Arc<Store>,
    iter: I,
}

impl<I: Iterator<Item = Result<CommitId, RevsetEvaluationError>>> Iterator
    for RevsetCommitIterator<I>
{
    type Item = Result<Commit, RevsetEvaluationError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|commit_id| {
            let commit_id = commit_id?;
            self.store
                .get_commit(&commit_id)
                .map_err(RevsetEvaluationError::Backend)
        })
    }
}

/// A set of extensions for revset evaluation.
pub struct RevsetExtensions {
    symbol_resolvers: Vec<Box<dyn SymbolResolverExtension>>,
    function_map: HashMap<&'static str, RevsetFunction>,
}

impl Default for RevsetExtensions {
    fn default() -> Self {
        Self::new()
    }
}

impl RevsetExtensions {
    pub fn new() -> Self {
        Self {
            symbol_resolvers: vec![],
            function_map: BUILTIN_FUNCTION_MAP.clone(),
        }
    }

    pub fn symbol_resolvers(&self) -> &[impl AsRef<dyn SymbolResolverExtension> + use<>] {
        &self.symbol_resolvers
    }

    pub fn add_symbol_resolver(&mut self, symbol_resolver: Box<dyn SymbolResolverExtension>) {
        self.symbol_resolvers.push(symbol_resolver);
    }

    pub fn add_custom_function(&mut self, name: &'static str, func: RevsetFunction) {
        match self.function_map.entry(name) {
            hash_map::Entry::Occupied(_) => {
                panic!("Conflict registering revset function '{name}'")
            }
            hash_map::Entry::Vacant(v) => v.insert(func),
        };
    }
}

/// Information needed to parse revset expression.
#[derive(Clone)]
pub struct RevsetParseContext<'a> {
    pub aliases_map: &'a RevsetAliasesMap,
    pub local_variables: HashMap<&'a str, ExpressionNode<'a>>,
    pub user_email: &'a str,
    pub date_pattern_context: DatePatternContext,
    pub extensions: &'a RevsetExtensions,
    pub workspace: Option<RevsetWorkspaceContext<'a>>,
}

impl<'a> RevsetParseContext<'a> {
    fn to_lowering_context(&self) -> LoweringContext<'a> {
        let RevsetParseContext {
            aliases_map: _,
            local_variables: _,
            user_email,
            date_pattern_context,
            extensions,
            workspace,
        } = *self;
        LoweringContext {
            user_email,
            date_pattern_context,
            extensions,
            workspace,
        }
    }
}

/// Information needed to transform revset AST into `UserRevsetExpression`.
#[derive(Clone)]
pub struct LoweringContext<'a> {
    user_email: &'a str,
    date_pattern_context: DatePatternContext,
    extensions: &'a RevsetExtensions,
    workspace: Option<RevsetWorkspaceContext<'a>>,
}

impl<'a> LoweringContext<'a> {
    pub fn user_email(&self) -> &'a str {
        self.user_email
    }

    pub fn date_pattern_context(&self) -> &DatePatternContext {
        &self.date_pattern_context
    }

    pub fn symbol_resolvers(&self) -> &'a [impl AsRef<dyn SymbolResolverExtension> + use<>] {
        self.extensions.symbol_resolvers()
    }
}

/// Workspace information needed to parse revset expression.
#[derive(Clone, Copy, Debug)]
pub struct RevsetWorkspaceContext<'a> {
    pub path_converter: &'a RepoPathUiConverter,
    pub workspace_name: &'a WorkspaceName,
}

/// Formats a string as symbol by quoting and escaping it if necessary.
///
/// Note that symbols may be substituted to user aliases. Use
/// [`format_string()`] to ensure that the provided string is resolved as a
/// tag/bookmark name, commit/change ID prefix, etc.
pub fn format_symbol(literal: &str) -> String {
    if revset_parser::is_identifier(literal) {
        literal.to_string()
    } else {
        format_string(literal)
    }
}

/// Formats a string by quoting and escaping it.
pub fn format_string(literal: &str) -> String {
    format!(r#""{}""#, dsl_util::escape_string(literal))
}

/// Formats a `name@remote` symbol, applies quoting and escaping if necessary.
pub fn format_remote_symbol(name: &str, remote: &str) -> String {
    let name = format_symbol(name);
    let remote = format_symbol(remote);
    format!("{name}@{remote}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use assert_matches::assert_matches;

    use super::*;

    fn parse(revset_str: &str) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
        parse_with_aliases(revset_str, [] as [(&str, &str); 0])
    }

    fn parse_with_workspace(
        revset_str: &str,
        workspace_name: &WorkspaceName,
    ) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
        parse_with_aliases_and_workspace(revset_str, [] as [(&str, &str); 0], workspace_name)
    }

    fn parse_with_aliases(
        revset_str: &str,
        aliases: impl IntoIterator<Item = (impl AsRef<str>, impl Into<String>)>,
    ) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
        let mut aliases_map = RevsetAliasesMap::new();
        for (decl, defn) in aliases {
            aliases_map.insert(decl, defn).unwrap();
        }
        let context = RevsetParseContext {
            aliases_map: &aliases_map,
            local_variables: HashMap::new(),
            user_email: "test.user@example.com",
            date_pattern_context: chrono::Utc::now().fixed_offset().into(),
            extensions: &RevsetExtensions::default(),
            workspace: None,
        };
        super::parse(&mut RevsetDiagnostics::new(), revset_str, &context)
    }

    fn parse_with_aliases_and_workspace(
        revset_str: &str,
        aliases: impl IntoIterator<Item = (impl AsRef<str>, impl Into<String>)>,
        workspace_name: &WorkspaceName,
    ) -> Result<Rc<UserRevsetExpression>, RevsetParseError> {
        // Set up pseudo context to resolve `workspace_name@` and `file(path)`
        let path_converter = RepoPathUiConverter::Fs {
            cwd: PathBuf::from("/"),
            base: PathBuf::from("/"),
        };
        let workspace_ctx = RevsetWorkspaceContext {
            path_converter: &path_converter,
            workspace_name,
        };
        let mut aliases_map = RevsetAliasesMap::new();
        for (decl, defn) in aliases {
            aliases_map.insert(decl, defn).unwrap();
        }
        let context = RevsetParseContext {
            aliases_map: &aliases_map,
            local_variables: HashMap::new(),
            user_email: "test.user@example.com",
            date_pattern_context: chrono::Utc::now().fixed_offset().into(),
            extensions: &RevsetExtensions::default(),
            workspace: Some(workspace_ctx),
        };
        super::parse(&mut RevsetDiagnostics::new(), revset_str, &context)
    }

    fn parse_with_modifier(
        revset_str: &str,
    ) -> Result<(Rc<UserRevsetExpression>, Option<RevsetModifier>), RevsetParseError> {
        parse_with_aliases_and_modifier(revset_str, [] as [(&str, &str); 0])
    }

    fn parse_with_aliases_and_modifier(
        revset_str: &str,
        aliases: impl IntoIterator<Item = (impl AsRef<str>, impl Into<String>)>,
    ) -> Result<(Rc<UserRevsetExpression>, Option<RevsetModifier>), RevsetParseError> {
        let mut aliases_map = RevsetAliasesMap::new();
        for (decl, defn) in aliases {
            aliases_map.insert(decl, defn).unwrap();
        }
        let context = RevsetParseContext {
            aliases_map: &aliases_map,
            local_variables: HashMap::new(),
            user_email: "test.user@example.com",
            date_pattern_context: chrono::Utc::now().fixed_offset().into(),
            extensions: &RevsetExtensions::default(),
            workspace: None,
        };
        super::parse_with_modifier(&mut RevsetDiagnostics::new(), revset_str, &context)
    }

    fn insta_settings() -> insta::Settings {
        let mut settings = insta::Settings::clone_current();
        // Collapse short "Thing(_,)" repeatedly to save vertical space and make
        // the output more readable.
        for _ in 0..4 {
            settings.add_filter(
                r"(?x)
                \b([A-Z]\w*)\(\n
                    \s*(.{1,60}),\n
                \s*\)",
                "$1($2)",
            );
        }
        settings
    }

    #[test]
    #[expect(clippy::redundant_clone)] // allow symbol.clone()
    fn test_revset_expression_building() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();
        let current_wc = UserRevsetExpression::working_copy(WorkspaceName::DEFAULT.to_owned());
        let foo_symbol = UserRevsetExpression::symbol("foo".to_string());
        let bar_symbol = UserRevsetExpression::symbol("bar".to_string());
        let baz_symbol = UserRevsetExpression::symbol("baz".to_string());

        insta::assert_debug_snapshot!(
            current_wc,
            @r#"CommitRef(WorkingCopy(WorkspaceNameBuf("default")))"#);
        insta::assert_debug_snapshot!(
            current_wc.heads(),
            @r#"Heads(CommitRef(WorkingCopy(WorkspaceNameBuf("default"))))"#);
        insta::assert_debug_snapshot!(
            current_wc.roots(),
            @r#"Roots(CommitRef(WorkingCopy(WorkspaceNameBuf("default"))))"#);
        insta::assert_debug_snapshot!(
            current_wc.parents(), @r#"
        Ancestors {
            heads: CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
            generation: 1..2,
        }
        "#);
        insta::assert_debug_snapshot!(
            current_wc.ancestors(), @r#"
        Ancestors {
            heads: CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.children(), @r#"
        Descendants {
            roots: CommitRef(Symbol("foo")),
            generation: 1..2,
        }
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.descendants(), @r#"
        Descendants {
            roots: CommitRef(Symbol("foo")),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.dag_range_to(&current_wc), @r#"
        DagRange {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
        }
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.connected(), @r#"
        DagRange {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(Symbol("foo")),
        }
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.range(&current_wc), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.negated(),
            @r#"NotIn(CommitRef(Symbol("foo")))"#);
        insta::assert_debug_snapshot!(
            foo_symbol.union(&current_wc), @r#"
        Union(
            CommitRef(Symbol("foo")),
            CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            UserRevsetExpression::union_all(&[]),
            @"None");
        insta::assert_debug_snapshot!(
            RevsetExpression::union_all(&[current_wc.clone()]),
            @r#"CommitRef(WorkingCopy(WorkspaceNameBuf("default")))"#);
        insta::assert_debug_snapshot!(
            RevsetExpression::union_all(&[current_wc.clone(), foo_symbol.clone()]),
            @r#"
        Union(
            CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
            CommitRef(Symbol("foo")),
        )
        "#);
        insta::assert_debug_snapshot!(
            RevsetExpression::union_all(&[
                current_wc.clone(),
                foo_symbol.clone(),
                bar_symbol.clone(),
            ]),
            @r#"
        Union(
            CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
            Union(
                CommitRef(Symbol("foo")),
                CommitRef(Symbol("bar")),
            ),
        )
        "#);
        insta::assert_debug_snapshot!(
            RevsetExpression::union_all(&[
                current_wc.clone(),
                foo_symbol.clone(),
                bar_symbol.clone(),
                baz_symbol.clone(),
            ]),
            @r#"
        Union(
            Union(
                CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
                CommitRef(Symbol("foo")),
            ),
            Union(
                CommitRef(Symbol("bar")),
                CommitRef(Symbol("baz")),
            ),
        )
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.intersection(&current_wc), @r#"
        Intersection(
            CommitRef(Symbol("foo")),
            CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            foo_symbol.minus(&current_wc), @r#"
        Difference(
            CommitRef(Symbol("foo")),
            CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            UserRevsetExpression::coalesce(&[]),
            @"None");
        insta::assert_debug_snapshot!(
            RevsetExpression::coalesce(&[current_wc.clone()]),
            @r#"CommitRef(WorkingCopy(WorkspaceNameBuf("default")))"#);
        insta::assert_debug_snapshot!(
            RevsetExpression::coalesce(&[current_wc.clone(), foo_symbol.clone()]),
            @r#"
        Coalesce(
            CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
            CommitRef(Symbol("foo")),
        )
        "#);
        insta::assert_debug_snapshot!(
            RevsetExpression::coalesce(&[
                current_wc.clone(),
                foo_symbol.clone(),
                bar_symbol.clone(),
            ]),
            @r#"
        Coalesce(
            CommitRef(WorkingCopy(WorkspaceNameBuf("default"))),
            Coalesce(
                CommitRef(Symbol("foo")),
                CommitRef(Symbol("bar")),
            ),
        )
        "#);
    }

    #[test]
    fn test_parse_revset() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();
        let main_workspace_name = WorkspaceNameBuf::from("main");
        let other_workspace_name = WorkspaceNameBuf::from("other");

        // Parse "@" (the current working copy)
        insta::assert_debug_snapshot!(
            parse("@").unwrap_err().kind(),
            @"WorkingCopyWithoutWorkspace");
        insta::assert_debug_snapshot!(
            parse("main@").unwrap(),
            @r#"CommitRef(WorkingCopy(WorkspaceNameBuf("main")))"#);
        insta::assert_debug_snapshot!(
            parse_with_workspace("@", &main_workspace_name).unwrap(),
            @r#"CommitRef(WorkingCopy(WorkspaceNameBuf("main")))"#);
        insta::assert_debug_snapshot!(
            parse_with_workspace("main@", &other_workspace_name).unwrap(),
            @r#"CommitRef(WorkingCopy(WorkspaceNameBuf("main")))"#);
        // "@" in function argument must be quoted
        insta::assert_debug_snapshot!(
            parse("author_name(foo@)").unwrap_err().kind(),
            @r#"Expression("Expected expression of string pattern")"#);
        insta::assert_debug_snapshot!(
            parse(r#"author_name("foo@")"#).unwrap(),
            @r#"Filter(AuthorName(Substring("foo@")))"#);
        // Parse a single symbol
        insta::assert_debug_snapshot!(
            parse("foo").unwrap(),
            @r#"CommitRef(Symbol("foo"))"#);
        // Default arguments for *bookmarks() are all ""
        insta::assert_debug_snapshot!(
            parse("bookmarks()").unwrap(),
            @r#"CommitRef(Bookmarks(Substring("")))"#);
        // Default argument for tags() is ""
        insta::assert_debug_snapshot!(
            parse("tags()").unwrap(),
            @r#"CommitRef(Tags(Substring("")))"#);
        insta::assert_debug_snapshot!(parse("remote_bookmarks()").unwrap(), @r#"
        CommitRef(
            RemoteBookmarks {
                bookmark_pattern: Substring(""),
                remote_pattern: Substring(""),
                remote_ref_state: None,
            },
        )
        "#);
        insta::assert_debug_snapshot!(parse("tracked_remote_bookmarks()").unwrap(), @r#"
        CommitRef(
            RemoteBookmarks {
                bookmark_pattern: Substring(""),
                remote_pattern: Substring(""),
                remote_ref_state: Some(Tracked),
            },
        )
        "#);
        insta::assert_debug_snapshot!(parse("untracked_remote_bookmarks()").unwrap(), @r#"
        CommitRef(
            RemoteBookmarks {
                bookmark_pattern: Substring(""),
                remote_pattern: Substring(""),
                remote_ref_state: Some(New),
            },
        )
        "#);
        // Parse a quoted symbol
        insta::assert_debug_snapshot!(
            parse("'foo'").unwrap(),
            @r#"CommitRef(Symbol("foo"))"#);
        // Parse the "parents" operator
        insta::assert_debug_snapshot!(parse("foo-").unwrap(), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 1..2,
        }
        "#);
        // Parse the "children" operator
        insta::assert_debug_snapshot!(parse("foo+").unwrap(), @r#"
        Descendants {
            roots: CommitRef(Symbol("foo")),
            generation: 1..2,
        }
        "#);
        // Parse the "ancestors" operator
        insta::assert_debug_snapshot!(parse("::foo").unwrap(), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 0..18446744073709551615,
        }
        "#);
        // Parse the "descendants" operator
        insta::assert_debug_snapshot!(parse("foo::").unwrap(), @r#"
        Descendants {
            roots: CommitRef(Symbol("foo")),
            generation: 0..18446744073709551615,
        }
        "#);
        // Parse the "dag range" operator
        insta::assert_debug_snapshot!(parse("foo::bar").unwrap(), @r#"
        DagRange {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(Symbol("bar")),
        }
        "#);
        // Parse the nullary "dag range" operator
        insta::assert_debug_snapshot!(parse("::").unwrap(), @"All");
        // Parse the "range" prefix operator
        insta::assert_debug_snapshot!(parse("..foo").unwrap(), @r#"
        Range {
            roots: Root,
            heads: CommitRef(Symbol("foo")),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(parse("foo..").unwrap(), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: VisibleHeads,
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(parse("foo..bar").unwrap(), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(Symbol("bar")),
            generation: 0..18446744073709551615,
        }
        "#);
        // Parse the nullary "range" operator
        insta::assert_debug_snapshot!(parse("..").unwrap(), @r"
        Range {
            roots: Root,
            heads: VisibleHeads,
            generation: 0..18446744073709551615,
        }
        ");
        // Parse the "negate" operator
        insta::assert_debug_snapshot!(
            parse("~ foo").unwrap(),
            @r#"NotIn(CommitRef(Symbol("foo")))"#);
        // Parse the "intersection" operator
        insta::assert_debug_snapshot!(parse("foo & bar").unwrap(), @r#"
        Intersection(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);
        // Parse the "union" operator
        insta::assert_debug_snapshot!(parse("foo | bar").unwrap(), @r#"
        Union(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);
        // Parse the "difference" operator
        insta::assert_debug_snapshot!(parse("foo ~ bar").unwrap(), @r#"
        Difference(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);
    }

    #[test]
    fn test_parse_revset_with_modifier() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            parse_with_modifier("all:foo").unwrap(), @r#"
        (
            CommitRef(Symbol("foo")),
            Some(All),
        )
        "#);

        // Top-level string pattern can't be parsed, which is an error anyway
        insta::assert_debug_snapshot!(
            parse_with_modifier(r#"exact:"foo""#).unwrap_err().kind(),
            @r#"NoSuchModifier("exact")"#);
    }

    #[test]
    fn test_parse_string_pattern() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            parse(r#"bookmarks("foo")"#).unwrap(),
            @r#"CommitRef(Bookmarks(Substring("foo")))"#);
        insta::assert_debug_snapshot!(
            parse(r#"bookmarks(exact:"foo")"#).unwrap(),
            @r#"CommitRef(Bookmarks(Exact("foo")))"#);
        insta::assert_debug_snapshot!(
            parse(r#"bookmarks(substring:"foo")"#).unwrap(),
            @r#"CommitRef(Bookmarks(Substring("foo")))"#);
        insta::assert_debug_snapshot!(
            parse(r#"bookmarks(bad:"foo")"#).unwrap_err().kind(),
            @r#"Expression("Invalid string pattern")"#);
        insta::assert_debug_snapshot!(
            parse(r#"bookmarks(exact::"foo")"#).unwrap_err().kind(),
            @r#"Expression("Expected expression of string pattern")"#);
        insta::assert_debug_snapshot!(
            parse(r#"bookmarks(exact:"foo"+)"#).unwrap_err().kind(),
            @r#"Expression("Expected expression of string pattern")"#);

        insta::assert_debug_snapshot!(
            parse(r#"tags("foo")"#).unwrap(),
            @r#"CommitRef(Tags(Substring("foo")))"#);
        insta::assert_debug_snapshot!(
            parse(r#"tags(exact:"foo")"#).unwrap(),
            @r#"CommitRef(Tags(Exact("foo")))"#);
        insta::assert_debug_snapshot!(
            parse(r#"tags(substring:"foo")"#).unwrap(),
            @r#"CommitRef(Tags(Substring("foo")))"#);
        insta::assert_debug_snapshot!(
            parse(r#"tags(bad:"foo")"#).unwrap_err().kind(),
            @r#"Expression("Invalid string pattern")"#);
        insta::assert_debug_snapshot!(
            parse(r#"tags(exact::"foo")"#).unwrap_err().kind(),
            @r#"Expression("Expected expression of string pattern")"#);
        insta::assert_debug_snapshot!(
            parse(r#"tags(exact:"foo"+)"#).unwrap_err().kind(),
            @r#"Expression("Expected expression of string pattern")"#);

        // String pattern isn't allowed at top level.
        assert_matches!(
            parse(r#"(exact:"foo")"#).unwrap_err().kind(),
            RevsetParseErrorKind::NotInfixOperator { .. }
        );
    }

    #[test]
    fn test_parse_revset_function() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            parse("parents(foo)").unwrap(), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 1..2,
        }
        "#);
        insta::assert_debug_snapshot!(
            parse("parents(\"foo\")").unwrap(), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 1..2,
        }
        "#);
        insta::assert_debug_snapshot!(
            parse("ancestors(parents(foo))").unwrap(), @r#"
        Ancestors {
            heads: Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 1..2,
            },
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(
            parse("parents(foo,foo)").unwrap_err().kind(), @r#"
        InvalidFunctionArguments {
            name: "parents",
            message: "Expected 1 arguments",
        }
        "#);
        insta::assert_debug_snapshot!(
            parse("root()").unwrap(),
            @"Root");
        assert!(parse("root(a)").is_err());
        insta::assert_debug_snapshot!(
            parse(r#"description("")"#).unwrap(),
            @r#"Filter(Description(Substring("")))"#);
        insta::assert_debug_snapshot!(
            parse("description(foo)").unwrap(),
            @r#"Filter(Description(Substring("foo")))"#);
        insta::assert_debug_snapshot!(
            parse("description(visible_heads())").unwrap_err().kind(),
            @r#"Expression("Expected expression of string pattern")"#);
        insta::assert_debug_snapshot!(
            parse("description(\"(foo)\")").unwrap(),
            @r#"Filter(Description(Substring("(foo)")))"#);
        assert!(parse("mine(foo)").is_err());
        insta::assert_debug_snapshot!(
            parse_with_workspace("empty()", WorkspaceName::DEFAULT).unwrap(),
            @"NotIn(Filter(File(All)))");
        assert!(parse_with_workspace("empty(foo)", WorkspaceName::DEFAULT).is_err());
        assert!(parse_with_workspace("file()", WorkspaceName::DEFAULT).is_err());
        insta::assert_debug_snapshot!(
            parse_with_workspace("files(foo)", WorkspaceName::DEFAULT).unwrap(),
            @r#"Filter(File(Pattern(PrefixPath("foo"))))"#);
        insta::assert_debug_snapshot!(
            parse_with_workspace("files(all())", WorkspaceName::DEFAULT).unwrap(),
            @"Filter(File(All))");
        insta::assert_debug_snapshot!(
            parse_with_workspace(r#"files(file:"foo")"#, WorkspaceName::DEFAULT).unwrap(),
            @r#"Filter(File(Pattern(FilePath("foo"))))"#);
        insta::assert_debug_snapshot!(
            parse_with_workspace("files(foo|bar&baz)", WorkspaceName::DEFAULT).unwrap(), @r#"
        Filter(
            File(
                UnionAll(
                    [
                        Pattern(PrefixPath("foo")),
                        Intersection(
                            Pattern(PrefixPath("bar")),
                            Pattern(PrefixPath("baz")),
                        ),
                    ],
                ),
            ),
        )
        "#);
        insta::assert_debug_snapshot!(parse("signed()").unwrap(), @"Filter(Signed)");
    }

    #[test]
    fn test_parse_revset_author_committer_functions() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            parse("author(foo)").unwrap(), @r#"
        Union(
            Filter(AuthorName(Substring("foo"))),
            Filter(AuthorEmail(Substring("foo"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            parse("author_name(foo)").unwrap(),
            @r#"Filter(AuthorName(Substring("foo")))"#);
        insta::assert_debug_snapshot!(
            parse("author_email(foo)").unwrap(),
            @r#"Filter(AuthorEmail(Substring("foo")))"#);

        insta::assert_debug_snapshot!(
            parse("committer(foo)").unwrap(), @r#"
        Union(
            Filter(CommitterName(Substring("foo"))),
            Filter(CommitterEmail(Substring("foo"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            parse("committer_name(foo)").unwrap(),
            @r#"Filter(CommitterName(Substring("foo")))"#);
        insta::assert_debug_snapshot!(
            parse("committer_email(foo)").unwrap(),
            @r#"Filter(CommitterEmail(Substring("foo")))"#);

        insta::assert_debug_snapshot!(
            parse("mine()").unwrap(),
            @r#"Filter(AuthorEmail(ExactI("test.user@example.com")))"#);
    }

    #[test]
    fn test_parse_revset_keyword_arguments() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            parse("remote_bookmarks(remote=foo)").unwrap(), @r#"
        CommitRef(
            RemoteBookmarks {
                bookmark_pattern: Substring(""),
                remote_pattern: Substring("foo"),
                remote_ref_state: None,
            },
        )
        "#);
        insta::assert_debug_snapshot!(
            parse("remote_bookmarks(foo, remote=bar)").unwrap(), @r#"
        CommitRef(
            RemoteBookmarks {
                bookmark_pattern: Substring("foo"),
                remote_pattern: Substring("bar"),
                remote_ref_state: None,
            },
        )
        "#);
        insta::assert_debug_snapshot!(
            parse("tracked_remote_bookmarks(foo, remote=bar)").unwrap(), @r#"
        CommitRef(
            RemoteBookmarks {
                bookmark_pattern: Substring("foo"),
                remote_pattern: Substring("bar"),
                remote_ref_state: Some(Tracked),
            },
        )
        "#);
        insta::assert_debug_snapshot!(
            parse("untracked_remote_bookmarks(foo, remote=bar)").unwrap(), @r#"
        CommitRef(
            RemoteBookmarks {
                bookmark_pattern: Substring("foo"),
                remote_pattern: Substring("bar"),
                remote_ref_state: Some(New),
            },
        )
        "#);
        insta::assert_debug_snapshot!(
            parse(r#"remote_bookmarks(remote=foo, bar)"#).unwrap_err().kind(),
            @r#"
        InvalidFunctionArguments {
            name: "remote_bookmarks",
            message: "Positional argument follows keyword argument",
        }
        "#);
        insta::assert_debug_snapshot!(
            parse(r#"remote_bookmarks("", foo, remote=bar)"#).unwrap_err().kind(),
            @r#"
        InvalidFunctionArguments {
            name: "remote_bookmarks",
            message: "Got multiple values for keyword \"remote\"",
        }
        "#);
        insta::assert_debug_snapshot!(
            parse(r#"remote_bookmarks(remote=bar, remote=bar)"#).unwrap_err().kind(),
            @r#"
        InvalidFunctionArguments {
            name: "remote_bookmarks",
            message: "Got multiple values for keyword \"remote\"",
        }
        "#);
        insta::assert_debug_snapshot!(
            parse(r#"remote_bookmarks(unknown=bar)"#).unwrap_err().kind(),
            @r#"
        InvalidFunctionArguments {
            name: "remote_bookmarks",
            message: "Unexpected keyword argument \"unknown\"",
        }
        "#);
    }

    #[test]
    fn test_expand_symbol_alias() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            parse_with_aliases("AB|c", [("AB", "a|b")]).unwrap(), @r#"
        Union(
            Union(
                CommitRef(Symbol("a")),
                CommitRef(Symbol("b")),
            ),
            CommitRef(Symbol("c")),
        )
        "#);

        // Alias can be substituted to string literal.
        insta::assert_debug_snapshot!(
            parse_with_aliases_and_workspace("files(A)", [("A", "a")], WorkspaceName::DEFAULT)
                .unwrap(),
            @r#"Filter(File(Pattern(PrefixPath("a"))))"#);

        // Alias can be substituted to string pattern.
        insta::assert_debug_snapshot!(
            parse_with_aliases("author_name(A)", [("A", "a")]).unwrap(),
            @r#"Filter(AuthorName(Substring("a")))"#);
        // However, parentheses are required because top-level x:y is parsed as
        // program modifier.
        insta::assert_debug_snapshot!(
            parse_with_aliases("author_name(A)", [("A", "(exact:a)")]).unwrap(),
            @r#"Filter(AuthorName(Exact("a")))"#);

        // Sub-expression alias cannot be substituted to modifier expression.
        insta::assert_debug_snapshot!(
            parse_with_aliases_and_modifier("A-", [("A", "all:a")]).unwrap_err().kind(),
            @r#"InAliasExpansion("A")"#);
    }

    #[test]
    fn test_expand_function_alias() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        // Pass string literal as parameter.
        insta::assert_debug_snapshot!(
            parse_with_aliases("F(a)", [("F(x)", "author_name(x)|committer_name(x)")]).unwrap(),
            @r#"
        Union(
            Filter(AuthorName(Substring("a"))),
            Filter(CommitterName(Substring("a"))),
        )
        "#);
    }

    #[test]
    fn test_optimize_subtree() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        // Check that transform_expression_bottom_up() never rewrites enum variant
        // (e.g. Range -> DagRange) nor reorders arguments unintentionally.

        insta::assert_debug_snapshot!(
            optimize(parse("parents(bookmarks() & all())").unwrap()), @r#"
        Ancestors {
            heads: CommitRef(Bookmarks(Substring(""))),
            generation: 1..2,
        }
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("children(bookmarks() & all())").unwrap()), @r#"
        Descendants {
            roots: CommitRef(Bookmarks(Substring(""))),
            generation: 1..2,
        }
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("ancestors(bookmarks() & all())").unwrap()), @r#"
        Ancestors {
            heads: CommitRef(Bookmarks(Substring(""))),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("descendants(bookmarks() & all())").unwrap()), @r#"
        Descendants {
            roots: CommitRef(Bookmarks(Substring(""))),
            generation: 0..18446744073709551615,
        }
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("(bookmarks() & all())..(all() & tags())").unwrap()), @r#"
        Range {
            roots: CommitRef(Bookmarks(Substring(""))),
            heads: CommitRef(Tags(Substring(""))),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("(bookmarks() & all())::(all() & tags())").unwrap()), @r#"
        DagRange {
            roots: CommitRef(Bookmarks(Substring(""))),
            heads: CommitRef(Tags(Substring(""))),
        }
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("heads(bookmarks() & all())").unwrap()),
            @r#"Heads(CommitRef(Bookmarks(Substring(""))))"#);
        insta::assert_debug_snapshot!(
            optimize(parse("roots(bookmarks() & all())").unwrap()),
            @r#"Roots(CommitRef(Bookmarks(Substring(""))))"#);

        insta::assert_debug_snapshot!(
            optimize(parse("predecessors(branches() & all())").unwrap()),
            @r###"Predecessors(CommitRef(Branches(Substring(""))))"###);

        insta::assert_debug_snapshot!(
            optimize(parse("latest(bookmarks() & all(), 2)").unwrap()), @r#"
        Latest {
            candidates: CommitRef(Bookmarks(Substring(""))),
            count: 2,
        }
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("present(foo ~ bar)").unwrap()), @r#"
        Present(
            Difference(
                CommitRef(Symbol("foo")),
                CommitRef(Symbol("bar")),
            ),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("present(bookmarks() & all())").unwrap()),
            @r#"Present(CommitRef(Bookmarks(Substring(""))))"#);

        insta::assert_debug_snapshot!(
            optimize(parse("at_operation(@-, bookmarks() & all())").unwrap()), @r#"
        AtOperation {
            operation: "@-",
            candidates: CommitRef(Bookmarks(Substring(""))),
        }
        "#);
        insta::assert_debug_snapshot!(
            optimize(Rc::new(RevsetExpression::WithinVisibility {
                candidates: parse("bookmarks() & all()").unwrap(),
                visible_heads: vec![CommitId::from_hex("012345")],
            })), @r#"
        WithinVisibility {
            candidates: CommitRef(Bookmarks(Substring(""))),
            visible_heads: [
                CommitId("012345"),
            ],
        }
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("~bookmarks() & all()").unwrap()),
            @r#"NotIn(CommitRef(Bookmarks(Substring(""))))"#);
        insta::assert_debug_snapshot!(
            optimize(parse("(bookmarks() & all()) | (all() & tags())").unwrap()), @r#"
        Union(
            CommitRef(Bookmarks(Substring(""))),
            CommitRef(Tags(Substring(""))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("(bookmarks() & all()) & (all() & tags())").unwrap()), @r#"
        Intersection(
            CommitRef(Bookmarks(Substring(""))),
            CommitRef(Tags(Substring(""))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("(bookmarks() & all()) ~ (all() & tags())").unwrap()), @r#"
        Difference(
            CommitRef(Bookmarks(Substring(""))),
            CommitRef(Tags(Substring(""))),
        )
        "#);
    }

    #[test]
    fn test_optimize_unchanged_subtree() {
        fn unwrap_union(
            expression: &UserRevsetExpression,
        ) -> (&Rc<UserRevsetExpression>, &Rc<UserRevsetExpression>) {
            match expression {
                RevsetExpression::Union(left, right) => (left, right),
                _ => panic!("unexpected expression: {expression:?}"),
            }
        }

        // transform_expression_bottom_up() should not recreate tree unnecessarily.
        let parsed = parse("foo-").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(&parsed, &optimized));

        let parsed = parse("bookmarks() | tags()").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(&parsed, &optimized));

        let parsed = parse("bookmarks() & tags()").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(&parsed, &optimized));

        // Only left subtree should be rewritten.
        let parsed = parse("(bookmarks() & all()) | tags()").unwrap();
        let optimized = optimize(parsed.clone());
        assert_matches!(
            unwrap_union(&optimized).0.as_ref(),
            RevsetExpression::CommitRef(RevsetCommitRef::Bookmarks(_))
        );
        assert!(Rc::ptr_eq(
            unwrap_union(&parsed).1,
            unwrap_union(&optimized).1
        ));

        // Only right subtree should be rewritten.
        let parsed = parse("bookmarks() | (all() & tags())").unwrap();
        let optimized = optimize(parsed.clone());
        assert!(Rc::ptr_eq(
            unwrap_union(&parsed).0,
            unwrap_union(&optimized).0
        ));
        assert_matches!(
            unwrap_union(&optimized).1.as_ref(),
            RevsetExpression::CommitRef(RevsetCommitRef::Tags(_))
        );
    }

    #[test]
    fn test_optimize_difference() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(optimize(parse("foo & ~bar").unwrap()), @r#"
        Difference(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("~foo & bar").unwrap()), @r#"
        Difference(
            CommitRef(Symbol("bar")),
            CommitRef(Symbol("foo")),
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("~foo & bar & ~baz").unwrap()), @r#"
        Difference(
            Difference(
                CommitRef(Symbol("bar")),
                CommitRef(Symbol("foo")),
            ),
            CommitRef(Symbol("baz")),
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("(all() & ~foo) & bar").unwrap()), @r#"
        Difference(
            CommitRef(Symbol("bar")),
            CommitRef(Symbol("foo")),
        )
        "#);

        // Binary difference operation should go through the same optimization passes.
        insta::assert_debug_snapshot!(
            optimize(parse("all() ~ foo").unwrap()),
            @r#"NotIn(CommitRef(Symbol("foo")))"#);
        insta::assert_debug_snapshot!(optimize(parse("foo ~ bar").unwrap()), @r#"
        Difference(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("(all() ~ foo) & bar").unwrap()), @r#"
        Difference(
            CommitRef(Symbol("bar")),
            CommitRef(Symbol("foo")),
        )
        "#);

        // Range expression.
        insta::assert_debug_snapshot!(optimize(parse("::foo & ~::bar").unwrap()), @r#"
        Range {
            roots: CommitRef(Symbol("bar")),
            heads: CommitRef(Symbol("foo")),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("~::foo & ::bar").unwrap()), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(Symbol("bar")),
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("foo..").unwrap()), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: VisibleHeads,
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("foo..bar").unwrap()), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(Symbol("bar")),
            generation: 0..18446744073709551615,
        }
        "#);

        // Double/triple negates.
        insta::assert_debug_snapshot!(optimize(parse("foo & ~~bar").unwrap()), @r#"
        Intersection(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("foo & ~~~bar").unwrap()), @r#"
        Difference(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("~(all() & ~foo) & bar").unwrap()), @r#"
        Intersection(
            CommitRef(Symbol("foo")),
            CommitRef(Symbol("bar")),
        )
        "#);

        // Should be better than '(all() & ~foo) & (all() & ~bar)'.
        insta::assert_debug_snapshot!(optimize(parse("~foo & ~bar").unwrap()), @r#"
        Difference(
            NotIn(CommitRef(Symbol("foo"))),
            CommitRef(Symbol("bar")),
        )
        "#);
    }

    #[test]
    fn test_optimize_not_in_ancestors() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        // '~(::foo)' is equivalent to 'foo..'.
        insta::assert_debug_snapshot!(optimize(parse("~(::foo)").unwrap()), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: VisibleHeads,
            generation: 0..18446744073709551615,
        }
        "#);

        // '~(::foo-)' is equivalent to 'foo-..'.
        insta::assert_debug_snapshot!(optimize(parse("~(::foo-)").unwrap()), @r#"
        Range {
            roots: Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 1..2,
            },
            heads: VisibleHeads,
            generation: 0..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("~(::foo--)").unwrap()), @r#"
        Range {
            roots: Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 2..3,
            },
            heads: VisibleHeads,
            generation: 0..18446744073709551615,
        }
        "#);

        // Bounded ancestors shouldn't be substituted.
        insta::assert_debug_snapshot!(optimize(parse("~ancestors(foo, 1)").unwrap()), @r#"
        NotIn(
            Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 0..1,
            },
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("~ancestors(foo-, 1)").unwrap()), @r#"
        NotIn(
            Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 1..2,
            },
        )
        "#);
    }

    #[test]
    fn test_optimize_filter_difference() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        // '~empty()' -> '~~file(*)' -> 'file(*)'
        insta::assert_debug_snapshot!(optimize(parse("~empty()").unwrap()), @"Filter(File(All))");

        // '& baz' can be moved into the filter node, and form a difference node.
        insta::assert_debug_snapshot!(
            optimize(parse("(author_name(foo) & ~bar) & baz").unwrap()), @r#"
        Intersection(
            Difference(
                CommitRef(Symbol("baz")),
                CommitRef(Symbol("bar")),
            ),
            Filter(AuthorName(Substring("foo"))),
        )
        "#);

        // '~set & filter()' shouldn't be substituted.
        insta::assert_debug_snapshot!(
            optimize(parse("~foo & author_name(bar)").unwrap()), @r#"
        Intersection(
            NotIn(CommitRef(Symbol("foo"))),
            Filter(AuthorName(Substring("bar"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("~foo & (author_name(bar) | baz)").unwrap()), @r#"
        Intersection(
            NotIn(CommitRef(Symbol("foo"))),
            AsFilter(
                Union(
                    Filter(AuthorName(Substring("bar"))),
                    CommitRef(Symbol("baz")),
                ),
            ),
        )
        "#);

        // Filter should be moved right of the intersection.
        insta::assert_debug_snapshot!(
            optimize(parse("author_name(foo) ~ bar").unwrap()), @r#"
        Intersection(
            NotIn(CommitRef(Symbol("bar"))),
            Filter(AuthorName(Substring("foo"))),
        )
        "#);
    }

    #[test]
    fn test_optimize_filter_intersection() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            optimize(parse("author_name(foo)").unwrap()),
            @r#"Filter(AuthorName(Substring("foo")))"#);

        insta::assert_debug_snapshot!(optimize(parse("foo & description(bar)").unwrap()), @r#"
        Intersection(
            CommitRef(Symbol("foo")),
            Filter(Description(Substring("bar"))),
        )
        "#);
        insta::assert_debug_snapshot!(optimize(parse("author_name(foo) & bar").unwrap()), @r#"
        Intersection(
            CommitRef(Symbol("bar")),
            Filter(AuthorName(Substring("foo"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("author_name(foo) & committer_name(bar)").unwrap()), @r#"
        Intersection(
            Filter(AuthorName(Substring("foo"))),
            Filter(CommitterName(Substring("bar"))),
        )
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & author_name(baz)").unwrap()), @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("foo")),
                Filter(Description(Substring("bar"))),
            ),
            Filter(AuthorName(Substring("baz"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("committer_name(foo) & bar & author_name(baz)").unwrap()), @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("bar")),
                Filter(CommitterName(Substring("foo"))),
            ),
            Filter(AuthorName(Substring("baz"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse_with_workspace(
                "committer_name(foo) & files(bar) & baz",
                WorkspaceName::DEFAULT).unwrap(),
            ), @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("baz")),
                Filter(CommitterName(Substring("foo"))),
            ),
            Filter(File(Pattern(PrefixPath("bar")))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse_with_workspace(
                "committer_name(foo) & files(bar) & author_name(baz)",
                WorkspaceName::DEFAULT).unwrap(),
            ), @r#"
        Intersection(
            Intersection(
                Filter(CommitterName(Substring("foo"))),
                Filter(File(Pattern(PrefixPath("bar")))),
            ),
            Filter(AuthorName(Substring("baz"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse_with_workspace(
                "foo & files(bar) & baz",
                WorkspaceName::DEFAULT).unwrap(),
            ), @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("foo")),
                CommitRef(Symbol("baz")),
            ),
            Filter(File(Pattern(PrefixPath("bar")))),
        )
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & author_name(baz) & qux").unwrap()), @r#"
        Intersection(
            Intersection(
                Intersection(
                    CommitRef(Symbol("foo")),
                    CommitRef(Symbol("qux")),
                ),
                Filter(Description(Substring("bar"))),
            ),
            Filter(AuthorName(Substring("baz"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & parents(author_name(baz)) & qux").unwrap()),
            @r#"
        Intersection(
            Intersection(
                Intersection(
                    CommitRef(Symbol("foo")),
                    Ancestors {
                        heads: Filter(AuthorName(Substring("baz"))),
                        generation: 1..2,
                    },
                ),
                CommitRef(Symbol("qux")),
            ),
            Filter(Description(Substring("bar"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("foo & description(bar) & parents(author_name(baz) & qux)").unwrap()),
            @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("foo")),
                Ancestors {
                    heads: Intersection(
                        CommitRef(Symbol("qux")),
                        Filter(AuthorName(Substring("baz"))),
                    ),
                    generation: 1..2,
                },
            ),
            Filter(Description(Substring("bar"))),
        )
        "#);

        // Symbols have to be pushed down to the innermost filter node.
        insta::assert_debug_snapshot!(
            optimize(parse("(a & author_name(A)) & (b & author_name(B)) & (c & author_name(C))").unwrap()),
            @r#"
        Intersection(
            Intersection(
                Intersection(
                    Intersection(
                        Intersection(
                            CommitRef(Symbol("a")),
                            CommitRef(Symbol("b")),
                        ),
                        CommitRef(Symbol("c")),
                    ),
                    Filter(AuthorName(Substring("A"))),
                ),
                Filter(AuthorName(Substring("B"))),
            ),
            Filter(AuthorName(Substring("C"))),
        )
        "#);
        insta::assert_debug_snapshot!(
            optimize(parse("(a & author_name(A)) & ((b & author_name(B)) & (c & author_name(C))) & d").unwrap()),
            @r#"
        Intersection(
            Intersection(
                Intersection(
                    Intersection(
                        Intersection(
                            CommitRef(Symbol("a")),
                            Intersection(
                                CommitRef(Symbol("b")),
                                CommitRef(Symbol("c")),
                            ),
                        ),
                        CommitRef(Symbol("d")),
                    ),
                    Filter(AuthorName(Substring("A"))),
                ),
                Filter(AuthorName(Substring("B"))),
            ),
            Filter(AuthorName(Substring("C"))),
        )
        "#);

        // 'all()' moves in to 'filter()' first, so 'A & filter()' can be found.
        insta::assert_debug_snapshot!(
            optimize(parse("foo & (all() & description(bar)) & (author_name(baz) & all())").unwrap()),
            @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("foo")),
                Filter(Description(Substring("bar"))),
            ),
            Filter(AuthorName(Substring("baz"))),
        )
        "#);

        // Filter node shouldn't move across at_operation() boundary.
        insta::assert_debug_snapshot!(
            optimize(parse("author_name(foo) & bar & at_operation(@-, committer_name(baz))").unwrap()),
            @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("bar")),
                AtOperation {
                    operation: "@-",
                    candidates: Filter(CommitterName(Substring("baz"))),
                },
            ),
            Filter(AuthorName(Substring("foo"))),
        )
        "#);
    }

    #[test]
    fn test_optimize_filter_subtree() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        insta::assert_debug_snapshot!(
            optimize(parse("(author_name(foo) | bar) & baz").unwrap()), @r#"
        Intersection(
            CommitRef(Symbol("baz")),
            AsFilter(
                Union(
                    Filter(AuthorName(Substring("foo"))),
                    CommitRef(Symbol("bar")),
                ),
            ),
        )
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("(foo | committer_name(bar)) & description(baz) & qux").unwrap()), @r#"
        Intersection(
            Intersection(
                CommitRef(Symbol("qux")),
                AsFilter(
                    Union(
                        CommitRef(Symbol("foo")),
                        Filter(CommitterName(Substring("bar"))),
                    ),
                ),
            ),
            Filter(Description(Substring("baz"))),
        )
        "#);

        insta::assert_debug_snapshot!(
            optimize(parse("(~present(author_name(foo) & bar) | baz) & qux").unwrap()), @r#"
        Intersection(
            CommitRef(Symbol("qux")),
            AsFilter(
                Union(
                    AsFilter(
                        NotIn(
                            AsFilter(
                                Present(
                                    Intersection(
                                        CommitRef(Symbol("bar")),
                                        Filter(AuthorName(Substring("foo"))),
                                    ),
                                ),
                            ),
                        ),
                    ),
                    CommitRef(Symbol("baz")),
                ),
            ),
        )
        "#);

        // Symbols have to be pushed down to the innermost filter node.
        insta::assert_debug_snapshot!(
            optimize(parse(
                "(a & (author_name(A) | 0)) & (b & (author_name(B) | 1)) & (c & (author_name(C) | 2))").unwrap()),
            @r#"
        Intersection(
            Intersection(
                Intersection(
                    Intersection(
                        Intersection(
                            CommitRef(Symbol("a")),
                            CommitRef(Symbol("b")),
                        ),
                        CommitRef(Symbol("c")),
                    ),
                    AsFilter(
                        Union(
                            Filter(AuthorName(Substring("A"))),
                            CommitRef(Symbol("0")),
                        ),
                    ),
                ),
                AsFilter(
                    Union(
                        Filter(AuthorName(Substring("B"))),
                        CommitRef(Symbol("1")),
                    ),
                ),
            ),
            AsFilter(
                Union(
                    Filter(AuthorName(Substring("C"))),
                    CommitRef(Symbol("2")),
                ),
            ),
        )
        "#);
    }

    #[test]
    fn test_optimize_ancestors() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        // Typical scenario: fold nested parents()
        insta::assert_debug_snapshot!(optimize(parse("foo--").unwrap()), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 2..3,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("::(foo---)").unwrap()), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 3..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("(::foo)---").unwrap()), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 3..18446744073709551615,
        }
        "#);

        // 'foo-+' is not 'foo'.
        insta::assert_debug_snapshot!(optimize(parse("foo---+").unwrap()), @r#"
        Descendants {
            roots: Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 3..4,
            },
            generation: 1..2,
        }
        "#);

        // For 'roots..heads', heads can be folded.
        insta::assert_debug_snapshot!(optimize(parse("foo..(bar--)").unwrap()), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: CommitRef(Symbol("bar")),
            generation: 2..18446744073709551615,
        }
        "#);
        // roots can also be folded, and the range expression is reconstructed.
        insta::assert_debug_snapshot!(optimize(parse("(foo--)..(bar---)").unwrap()), @r#"
        Range {
            roots: Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 2..3,
            },
            heads: CommitRef(Symbol("bar")),
            generation: 3..18446744073709551615,
        }
        "#);
        // Bounded ancestors shouldn't be substituted to range.
        insta::assert_debug_snapshot!(
            optimize(parse("~ancestors(foo, 2) & ::bar").unwrap()), @r#"
        Difference(
            Ancestors {
                heads: CommitRef(Symbol("bar")),
                generation: 0..18446744073709551615,
            },
            Ancestors {
                heads: CommitRef(Symbol("foo")),
                generation: 0..2,
            },
        )
        "#);

        // If inner range is bounded by roots, it cannot be merged.
        // e.g. '..(foo..foo)' is equivalent to '..none()', not to '..foo'
        insta::assert_debug_snapshot!(optimize(parse("(foo..bar)--").unwrap()), @r#"
        Ancestors {
            heads: Range {
                roots: CommitRef(Symbol("foo")),
                heads: CommitRef(Symbol("bar")),
                generation: 0..18446744073709551615,
            },
            generation: 2..3,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("foo..(bar..baz)").unwrap()), @r#"
        Range {
            roots: CommitRef(Symbol("foo")),
            heads: Range {
                roots: CommitRef(Symbol("bar")),
                heads: CommitRef(Symbol("baz")),
                generation: 0..18446744073709551615,
            },
            generation: 0..18446744073709551615,
        }
        "#);

        // Ancestors of empty generation range should be empty.
        insta::assert_debug_snapshot!(
            optimize(parse("ancestors(ancestors(foo), 0)").unwrap()), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 0..0,
        }
        "#
        );
        insta::assert_debug_snapshot!(
            optimize(parse("ancestors(ancestors(foo, 0))").unwrap()), @r#"
        Ancestors {
            heads: CommitRef(Symbol("foo")),
            generation: 0..0,
        }
        "#
        );
    }

    #[test]
    fn test_optimize_descendants() {
        let settings = insta_settings();
        let _guard = settings.bind_to_scope();

        // Typical scenario: fold nested children()
        insta::assert_debug_snapshot!(optimize(parse("foo++").unwrap()), @r#"
        Descendants {
            roots: CommitRef(Symbol("foo")),
            generation: 2..3,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("(foo+++)::").unwrap()), @r#"
        Descendants {
            roots: CommitRef(Symbol("foo")),
            generation: 3..18446744073709551615,
        }
        "#);
        insta::assert_debug_snapshot!(optimize(parse("(foo::)+++").unwrap()), @r#"
        Descendants {
            roots: CommitRef(Symbol("foo")),
            generation: 3..18446744073709551615,
        }
        "#);

        // 'foo+-' is not 'foo'.
        insta::assert_debug_snapshot!(optimize(parse("foo+++-").unwrap()), @r#"
        Ancestors {
            heads: Descendants {
                roots: CommitRef(Symbol("foo")),
                generation: 3..4,
            },
            generation: 1..2,
        }
        "#);

        // TODO: Inner Descendants can be folded into DagRange. Perhaps, we can rewrite
        // 'x::y' to 'x:: & ::y' first, so the common substitution rule can handle both
        // 'x+::y' and 'x+ & ::y'.
        insta::assert_debug_snapshot!(optimize(parse("(foo++)::bar").unwrap()), @r#"
        DagRange {
            roots: Descendants {
                roots: CommitRef(Symbol("foo")),
                generation: 2..3,
            },
            heads: CommitRef(Symbol("bar")),
        }
        "#);
    }

    #[test]
    fn test_escape_string_literal() {
        // Valid identifiers don't need quoting
        assert_eq!(format_symbol("foo"), "foo");
        assert_eq!(format_symbol("foo.bar"), "foo.bar");

        // Invalid identifiers need quoting
        assert_eq!(format_symbol("foo@bar"), r#""foo@bar""#);
        assert_eq!(format_symbol("foo bar"), r#""foo bar""#);
        assert_eq!(format_symbol(" foo "), r#"" foo ""#);
        assert_eq!(format_symbol("(foo)"), r#""(foo)""#);
        assert_eq!(format_symbol("all:foo"), r#""all:foo""#);

        // Some characters also need escaping
        assert_eq!(format_symbol("foo\"bar"), r#""foo\"bar""#);
        assert_eq!(format_symbol("foo\\bar"), r#""foo\\bar""#);
        assert_eq!(format_symbol("foo\\\"bar"), r#""foo\\\"bar""#);
        assert_eq!(format_symbol("foo\nbar"), r#""foo\nbar""#);

        // Some characters don't technically need escaping, but we escape them for
        // clarity
        assert_eq!(format_symbol("foo\"bar"), r#""foo\"bar""#);
        assert_eq!(format_symbol("foo\\bar"), r#""foo\\bar""#);
        assert_eq!(format_symbol("foo\\\"bar"), r#""foo\\\"bar""#);
        assert_eq!(format_symbol("foo \x01 bar"), r#""foo \x01 bar""#);
    }

    #[test]
    fn test_escape_remote_symbol() {
        assert_eq!(format_remote_symbol("foo", "bar"), "foo@bar");
        assert_eq!(
            format_remote_symbol(" foo ", "bar:baz"),
            r#"" foo "@"bar:baz""#
        );
    }
}
