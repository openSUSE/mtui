//! Refhost query model and search engine.
//!
//! Ported from upstream `mtui/hosts/refhost/`:
//! - [`models`] — the [`Attributes`] search query + its `testplatform` grammar
//!   parser (upstream `models.py::Attributes`).
//! - [`store`] — the [`Refhosts`] search engine over a loaded `refhosts.yml`
//!   (upstream `store.py::Refhosts`, search surface only).
//!
//! The `refhosts.yml` *row* schema ([`mtui_types::Host`] etc.) and the pure
//! document loader ([`mtui_types::load_refhosts`]) live in `mtui-types`; this
//! module builds the query/search layer on top.

pub mod models;
pub mod store;

pub use models::Attributes;
pub use store::Refhosts;
