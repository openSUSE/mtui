//! `update`, `product`, `origin` (`$defs/update`, `$defs/product` of the
//! TeReGen report-document schema, v1.0).

use serde::{Deserialize, Serialize};

/// What the update is (`update`), never written by a tester.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Update {
    /// Packager, resolved to an email address where possible.
    pub packager: String,
    /// Source package names this update rebuilds (schema `minItems: 1`).
    pub source_packages: Vec<String>,
    pub origin: Origin,
    /// Products this update ships to (schema `minItems: 1`).
    pub products: Vec<Product>,
    /// Patchinfo category verbatim, such as `security` or `recommended`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Patchinfo rating verbatim, such as `critical` or `moderate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    /// Updateinfo entries rolled into a Product Increment, in log order.
    /// Required by the schema's second `allOf` conditional when `kind: pi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patches: Option<Vec<Patch>>,
}

/// One `update.patches[]` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub id: String,
    pub title: String,
}

/// One `update.products[]` entry (`$defs/product`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Product {
    pub name: String,
    pub version: String,
    /// Schema `minItems: 1`.
    pub archs: Vec<String>,
}

/// Where the update came from, shaped by `workflow` (`update.origin`).
///
/// All six keys are optional — the schema declares no `required` here, and
/// the observed shape mixes `request`/`project`/`review_url` (`obs`) with
/// `pull_request`/`api`/`commit` (`gitea`). Modelled as one struct with
/// everything optional rather than an untagged enum keyed on `workflow`: the
/// schema permits a mix, and an untagged enum would silently pick the wrong
/// arm (see the plan's "Alternatives considered").
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Origin {
    /// OBS/IBS request number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<i64>,
    /// OBS/IBS project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Human-facing request page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_url: Option<String>,
    /// Gitea pull request, human-facing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<String>,
    /// Gitea pull request, API endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Head commit of the pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}
