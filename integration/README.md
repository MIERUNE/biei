# Stack integration tests

Cross-service tests belong here. The first target is the public contract Biei
uses from Ishikari: style rewriting, TileJSON/resource URLs, glyph and sprite
representation metadata, and a complete static render through both services.

Unit and simulator tests remain in their service workspaces. Integration tests
must treat each service as an independently deployable process and must not use
private Rust APIs to bypass the HTTP contract.
