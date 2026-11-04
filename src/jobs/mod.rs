//! The work the scheduled and manual jobs perform.
//!
//! [`daily`] is the bulk pull and the indicator rebuild. [`backfill`] is the free work beside
//! it: history aggregated from `BarHourly`, and raw closes from the 5-minute tape. Both are
//! library modules rather than code inside a binary so the scheduler, the `/refresh` endpoint
//! and `mmds-daily` all run the same implementation; a second copy is how the ASX and US
//! pulls drift apart.

pub mod backfill;
pub mod daily;
