# Changelog

## [0.2.0](https://github.com/arc-labs-ai/brain-db/compare/v0.1.0...v0.2.0) (2026-08-03)


### Features

* fixed release cycle package dependency ([de79988](https://github.com/arc-labs-ai/brain-db/commit/de799886e134998fc6ae9bf28b5355f6b129e257))


### Bug Fixes

* **ci:** check out the repo before the local setup-rust action ([3dac3ac](https://github.com/arc-labs-ai/brain-db/commit/3dac3ac444f0bd42a0115283bf52ae710d5262e0))
* **deps:** bump crossbeam-epoch to 0.9.20 for RUSTSEC-2026-0204 ([f64b782](https://github.com/arc-labs-ai/brain-db/commit/f64b782c9a30c8c0f99600816a3c451ba5341330))
* **index:** join the lexical indexers at shard teardown instead of detaching ([bee997f](https://github.com/arc-labs-ai/brain-db/commit/bee997f38fed12c82c2dce9b6b77d6682e27be53))
* **release:** simple release-type for the workspace + silence Node 20 warnings ([5ecdd3b](https://github.com/arc-labs-ai/brain-db/commit/5ecdd3be6e4d8d4119c5d7e70d4cbfbe51ffc95e))
* **shutdown:** drain shards within budget by cancelling detached tasks ([890ec47](https://github.com/arc-labs-ai/brain-db/commit/890ec472a9bdc747f733479c214c25419688f8e2))
* **write:** persist a dateless Event instead of rejecting it ([39baeec](https://github.com/arc-labs-ai/brain-db/commit/39baeec6033c551baeac4968e76209c67589ee9c))
