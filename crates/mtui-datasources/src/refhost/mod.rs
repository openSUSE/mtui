//! Refhost query model and search engine.
//!
//! Ported from upstream `mtui/hosts/refhost/`:
//! - [`models`] — the [`Attributes`] search query + its `testplatform` grammar
//!   parser (upstream `models.py::Attributes`).
//! - [`store`] — the [`Refhosts`] search engine over a loaded `refhosts.yml`
//!   (upstream `store.py::Refhosts`, search surface only).
//! - [`resolvers`] — the resolver chain ([`PathResolver`]/[`HttpsResolver`]) and
//!   the config-driven [`RefhostsFactory`] that decides *where* `refhosts.yml`
//!   comes from (upstream `resolvers.py` + the `_RefhostsFactory` binding).
//! - [`verify`] — advisory product-drift comparison between a detected
//!   [`System`](mtui_types::System) and a `refhosts.yml` [`Host`](mtui_types::Host)
//!   row, yielding a [`ProductDiff`] (upstream `verify.py`).
//!
//! The `refhosts.yml` *row* schema ([`mtui_types::Host`] etc.) and the pure
//! document loader ([`mtui_types::load_refhosts`]) live in `mtui-types`; this
//! module builds the query/search layer on top.

pub mod models;
pub mod resolvers;
pub mod store;
pub mod verify;

pub use models::Attributes;
pub use resolvers::{HttpsResolver, PathResolver, RefhostsFactory, ResolveConfig, Resolver};
pub use store::Refhosts;
pub use verify::{ProductDiff, compare, normalize_version};
