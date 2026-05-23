//! Cross-cutting wire types shared by multiple op families. Primitives
//! that show up in many requests (`MemoryKindWire`, `EdgeKindWire`,
//! `PlanStrategy`, `PlanState`, `ForgetMode`) live in `primitives`;
//! cross-cutting enums shared by responses and events (`EventType`,
//! `StageKind`, `RetrieverNameWire`, the `ErrorCategory`/`ErrorCode`
//! wire mirrors, …) live in `enums`. Putting them here avoids each op
//! file re-declaring the same wire encoding.

pub mod enums;
pub mod primitives;
