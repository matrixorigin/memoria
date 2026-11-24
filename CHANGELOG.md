# Changelog

All notable changes to this project will be documented in this file.

## [0.5.3] - 2026-11-24

### Added

- Add atomic, owner-scoped idempotency receipts for memory writes (#310).
- Add cached-only snapshot listing controls and a complete structured MCP capability catalog (#342, #325).
- Support configurable content filtering for trusted owner requests (#372).
- Preserve complete conversations within the observe extraction budget (#512).
- Add a Hermes memory provider plugin, distributed separately from the Rust binaries (#279, #280, #284, #302).

### Fixed

- Keep full-text scores separate from vector scores, apply feedback to graph retrieval, and preserve long-query retrieval (#260, #305, #262).
- Fall back to the existing LIKE query when MatrixOne reports a known unusable FULLTEXT index; preserve literal topic matches and avoid truncated purge results (#451, #506, #298, #319).
- Preserve routed database session consistency, coordinate scope initialization and invalidation, and discard connections after interrupted statement preparation (#322, #324, #328, #331).
- Repair call-log columns in versioned user databases (#283).
- Enforce scoped memory ownership, preserve correction metadata, and validate content and scope lengths consistently (#401, #318, #290, #316, #399, #458).
- Avoid treating transient query results as durable observations or creating safety snapshots for no-op purges (#288, #291).
- Improve credential and PII filtering while preserving ordinary prose and numeric identifiers; redact credential URLs and mask Unicode API keys safely (#278, #306, #394, #397, #404, #457, #504).
- Validate embedding responses and vectors, recover truncated responses, reject non-finite values, and keep mock embeddings distinct (#277, #304, #491).
- Return MCP tool failures as `isError` results, handle control notifications without error accounting, and validate JSON-RPC versions and request IDs while preserving stdio recovery (#257, #258, #389, #393, #460).
- Keep remote MCP resource identifiers within one REST path segment and reject invalid memory types (#407, #309).
- Validate snapshot pagination, preserve UTF-8 previews and literal deletion selectors, and reject blank deletion prefixes and query cursors (#315, #365, #369, #370, #414).
- Validate batch import fields, personal user IDs, admin pagination and group deletion; redact credentials in admin URLs (#335, #418, #459).
- Preserve unreadable MCP configuration files, derive shared database settings from `--db-url`, and propagate benchmark failures (#293, #303, #294).

### Compatibility and Release Scope

- Invalid inputs that were previously accepted or silently ignored now fail explicitly, including blank memory content, overlong scope identifiers, `grp_`-prefixed personal user IDs, blank snapshot deletion prefixes, and unsupported batch import fields.
- The `v0.5.3` tag publishes Rust binaries and Docker images. OpenClaw npm and Python SDK packages retain their own versions and require their separate release workflows; fixes in their source trees are not automatically published to npm or PyPI.
- Existing MatrixOne upgrade and legacy snapshot considerations remain documented in the [compatibility notes](docs/legacy-snapshot-compatibility.md).

## [0.5.2] - 2026-09-11

### 🐛 Bug Fixes

- **auth**: Add owner-scoped master authority (#250) ([bc55ee3](https://github.com/matrixorigin/Memoria/commit/bc55ee355f236cafd0701a107f30e44b1711b3c8))
- **openclaw**: Ship runnable npm package and preserve search scores (#249) ([61001bc](https://github.com/matrixorigin/Memoria/commit/61001bc8fa47e40eecf907812ad4a9932e59b275))
- **snapshot**: Preserve data when restoring pre-subject_id snapshots (#248) ([a2e1e25](https://github.com/matrixorigin/Memoria/commit/a2e1e254060b72b74813387dcd357f53cdfeeb0a))
- **mcp**: Handle ping requests (#237) ([6279342](https://github.com/matrixorigin/Memoria/commit/62793426103d772f856209a3bd4b3ac8fb783a24))

### 📚 Documentation

- Fix broken star history chart (#233) ([85f6c3f](https://github.com/matrixorigin/Memoria/commit/85f6c3fba0c939052500250c42375e460a0bca02))

### 🚀 Features

- **auth**: Add scoped API keys for Astra (#244) ([58012cf](https://github.com/matrixorigin/Memoria/commit/58012cffaea2ab610f4847a877a8dad2048d1eb5))

### Release Notes

- OpenClaw npm and Python SDK packages use separate release workflows; this release tag does not publish them.
- The legacy snapshot restore fix does not resolve the MatrixOne upgrade failure observed on `v4.2.1-d2393868a-2026-08-28`. See [compatibility and verification notes](docs/legacy-snapshot-compatibility.md) before upgrading a production database.

## [0.5.1] - 2026-08-26

### 🐛 Bug Fixes

- **storage**: Handle MatrixOne aggregate and nullable parameter compatibility (#239) ([54566bc](https://github.com/matrixorigin/Memoria/commit/54566bcf33546e7fbf9a6084e16eaafefa35fdfb))

### 👷 CI

- Increase MatrixOne memory limit (#234) ([0e15b55](https://github.com/matrixorigin/Memoria/commit/0e15b55e20552824b93b091b379039fc9e53444f))
## [0.5.0] - 2026-08-26

### ⚡ Performance

- **snapshot**: Optimize snapshot listing (#202) ([3207091](https://github.com/matrixorigin/Memoria/commit/320709171b85444c5c9115cea6f09fdba98f65b1))

### 🐛 Bug Fixes

- **snapshot**: Read SHOW SNAPSHOTS columns case-insensitively for MatrixOne v4.x (#226) ([4676239](https://github.com/matrixorigin/Memoria/commit/4676239597039f4ad6564e18990dde4f3f34cebe))
- Warnings in make test command (#221) ([63f0289](https://github.com/matrixorigin/Memoria/commit/63f0289da1fb3744523b29860f7e83be88871f28))
- Release python sdk ci (#207) ([37ca1e7](https://github.com/matrixorigin/Memoria/commit/37ca1e773af608c08b00a14f3814752c43df6c41))

### 🚀 Features

- Restore filtered fulltext memory search (#232) ([efd3d65](https://github.com/matrixorigin/Memoria/commit/efd3d6515969971dfa894737272b8317bcb643e7))
- Add structured memory queries (#228) ([ca9394d](https://github.com/matrixorigin/Memoria/commit/ca9394d6015f4394e9ccccf43588ea5e0c7b4395))
- Support extra metadata in batch write (#223) ([54c9114](https://github.com/matrixorigin/Memoria/commit/54c9114fd6888e11821edc2ee9acd570c17c5ee3))
- Add subject_id isolation and memory_types filtering for retrieve/search (#215) ([2446feb](https://github.com/matrixorigin/Memoria/commit/2446febc9f458cdc82bcec7b984c1e5f17f26ea5))
- Add memoria python sdk (#206) ([a2e659c](https://github.com/matrixorigin/Memoria/commit/a2e659cae58d2a42a9ee9e05f69d84c48a4f3f94))
- Add explicit branch access for memory operations (#203) ([7b2b372](https://github.com/matrixorigin/Memoria/commit/7b2b3729568546530c11000c45894dcf2f49d87b))
- **memory**: Add selective branch pick support (#193) ([4d16236](https://github.com/matrixorigin/Memoria/commit/4d16236cdc14f2ba5aff11266330b1caf8a29369))
## [0.4.0] - 2026-05-11

### 🐛 Bug Fixes

- Trivial fix for pro function (#201) ([caa3d79](https://github.com/matrixorigin/Memoria/commit/caa3d79438c71e6aebbcff5abc875728a80e3962))

### 📚 Documentation

- Add citation instructions (#197) ([a7c3547](https://github.com/matrixorigin/Memoria/commit/a7c3547264a14ac1746205d819b24381a68502a3))
- Add MatrixOne research paper reference (#196) ([a5d2786](https://github.com/matrixorigin/Memoria/commit/a5d2786c61312185e668bf4c98fdb83be333e454))

### 🚀 Features

- Group collaboration API with per-member branch isolation (#198) ([357d44c](https://github.com/matrixorigin/Memoria/commit/357d44c126419912b6f6a1c76bb5c1ef143270b3))
## [0.3.3] - 2026-04-22

### 🐛 Bug Fixes

- **cli**: Normalize MCP tool name; MatrixOne-safe srv_user_stats upsert (#192) ([f18069c](https://github.com/matrixorigin/Memoria/commit/f18069c01438ff85594254fd8e912603501fe8e5))

### 🚀 Features

- **memory**: Unify session scope across memory interfaces (#190) ([f504989](https://github.com/matrixorigin/Memoria/commit/f5049897bb37a327ff6c45b9e6d53bf1cab7e0a6))
- Add multi-db pool budget sizing (#191) ([79c1fb0](https://github.com/matrixorigin/Memoria/commit/79c1fb060bfaf25af53d85dc0827d7629c6db6fe))

### 🧪 Testing

- Migrate API and MCP e2e to multi-db (#189) ([b34717d](https://github.com/matrixorigin/Memoria/commit/b34717d770ed58d0caa8d86f4b4247c0ad6c4a3e))
## [0.3.2] - 2026-04-17

### 🐛 Bug Fixes

- **multidb**: Default fresh deployments to multi-db (#188) ([de98df6](https://github.com/matrixorigin/Memoria/commit/de98df6b64fb692f2bf1711136783178ccefc3ce))
- **storage**: Remove routed USE db dependency (#187) ([11603af](https://github.com/matrixorigin/Memoria/commit/11603afa9f7e11599a6f8a09db04c725133f6024))
## [0.3.1] - 2026-04-16

### ⚡ Performance

- **storage**: Harden multi-db pools and speed legacy migration (#180) ([3a1c7a2](https://github.com/matrixorigin/Memoria/commit/3a1c7a2ecfbc6e2924d0baed7ceb34d9f14082be))

### 🐛 Bug Fixes

- **memory**: Enforce strict session retrieval semantics (#185) ([1f99f39](https://github.com/matrixorigin/Memoria/commit/1f99f396c345edd8fb0bd076334e2462aa078510))

### 🚀 Features

- **memory**: Add session-scoped purge (#186) ([07c0e7f](https://github.com/matrixorigin/Memoria/commit/07c0e7fae0aa2d4063b15362709a2986a8a2ab7d))
- Add ops metrics (#177) ([897d6d5](https://github.com/matrixorigin/Memoria/commit/897d6d5475351a8f8ec4636fc714190bd1fb6808))
## [0.3.0] - 2026-04-13

### ⚡ Performance

- Improve store (#151) ([3ef1e04](https://github.com/matrixorigin/Memoria/commit/3ef1e04199ccd6337c2616a86b01e29b7302cb13))
- Improve memory list (#150) ([0fdd5bc](https://github.com/matrixorigin/Memoria/commit/0fdd5bc3c00b24603b6bc056412b436d9087a94e))
- Optimize list memories again (#149) ([febb16f](https://github.com/matrixorigin/Memoria/commit/febb16f6a386b4fa380de215faec6479beb5efa8))
- Optimize list (#148) ([a4d31f7](https://github.com/matrixorigin/Memoria/commit/a4d31f7092560710e860b84066de92b9f755d289))
- Async batched edit-log writer with unified flush path (#134) ([e3718c2](https://github.com/matrixorigin/Memoria/commit/e3718c2604cef5c600e0db01ceab637db93082cc))
- Optimize db pool usage (#83) ([53c0242](https://github.com/matrixorigin/Memoria/commit/53c024297b80cf933eb0a1ce5a9ee781c2b45b2c))

### 🐛 Bug Fixes

- **runtime**: Harden multi-db rollout and metrics rollups (#173) ([fbd4d48](https://github.com/matrixorigin/Memoria/commit/fbd4d48fe43412211a980b74f4139bbc876d0545))
- Finalize per-user DB isolation rollout (#172) ([b1917ef](https://github.com/matrixorigin/Memoria/commit/b1917ef25c3bac49b0e772933ed8b17af526d0e2))
- Migration drops mem_branches on every startup + UTF-8 panic in diff preview (#171) ([e1eab73](https://github.com/matrixorigin/Memoria/commit/e1eab735ba1857af5db6a1e8f4f049a8fdba0a56))
- **storage**: Use current tenant for account snapshots (#169) ([72802c1](https://github.com/matrixorigin/Memoria/commit/72802c1b24e2d5eb6d6c9429bad0452dcf7899cf))
- **ci**: Fix npm publish workflow for Linux runners (#164) ([9f746ac](https://github.com/matrixorigin/Memoria/commit/9f746ac857f5a1c2acf3db6a737406982108473b))
- **openclaw-plugin**: Pass OpenClaw security scan by isolating child_process usage (#163) ([49a6e4a](https://github.com/matrixorigin/Memoria/commit/49a6e4add4e81f15714a161c83ed6ced80387fb6))
- Improve pool diagnostics and serialize db verification tests (#159) ([3c57be7](https://github.com/matrixorigin/Memoria/commit/3c57be736efc417a861dee3c7aa48b750bfdaf66))
- No statistical data for the retrieval API called via MCP (#157) ([4aa7a00](https://github.com/matrixorigin/Memoria/commit/4aa7a007fa995c9a599c8e02ad12fcc3b97a3920))
- Big purge (#154) ([17265aa](https://github.com/matrixorigin/Memoria/commit/17265aabad8ce873f3ac995933bf18124c55beb4))
- Background governance (#152) ([c712384](https://github.com/matrixorigin/Memoria/commit/c712384fee1cb4bd966116226a2fea22de0f5b3a))
- Null processing workaround due to matrixone 3.0.8 bug (#136) ([7c77de1](https://github.com/matrixorigin/Memoria/commit/7c77de191d14b886957d0e6600d6e93761b09e8f))
- Multi-table DELETE+LIMIT syntax error and inaccurate index comment in cleanup_orphan_stats (#115) ([8553bf0](https://github.com/matrixorigin/Memoria/commit/8553bf027eba330dc3accb3bbc28a710aeb514af))
- Isolate DB pools, drop-on-full entity queue, embedding concurren… (#112) ([fd92c54](https://github.com/matrixorigin/Memoria/commit/fd92c54004fecedf384ea84098a773b1130cb0db))
- Enable user level metrics (#111) ([8f61732](https://github.com/matrixorigin/Memoria/commit/8f61732469f12ced4c1206cd65d0854a9f57b201))
- Fail fast when local embedding support is unavailable (#103) ([4320764](https://github.com/matrixorigin/Memoria/commit/432076411a39bc74ae790d60716b9b450c8c2ff1))
- **openclaw**: Fail fast for unsupported local embeddings (#85) ([0365491](https://github.com/matrixorigin/Memoria/commit/03654910b7a79bb92cf51ce268afa70e6c6abf91))
- Code agent tool header (#96) ([2bb2e2f](https://github.com/matrixorigin/Memoria/commit/2bb2e2fa778e7506cebe9b1cd62955015ff7a78d))
- Stop words (#77) ([1ce8be9](https://github.com/matrixorigin/Memoria/commit/1ce8be99591cfc3b5efc048da2e05f27dccf981a))

### 👷 CI

- **npm**: Auto-determine version from npm registry (#165) ([89e8281](https://github.com/matrixorigin/Memoria/commit/89e8281913b6d3547fe895c6bb81c902fa4b1309))
- **Mergify**: Configuration update (#142) ([6e9f5c6](https://github.com/matrixorigin/Memoria/commit/6e9f5c693061a6a9a57fcd9e8ffc9a10ec9a069a))
- **Mergify**: Configuration update (#143) ([bf2335e](https://github.com/matrixorigin/Memoria/commit/bf2335ec56ada9c978761240a8c4c88e7c36de65))
- Add GitHub Actions workflow for npm publishing + update npm package version to 0.4.2 (#109) ([61cb0d2](https://github.com/matrixorigin/Memoria/commit/61cb0d280d4bd69aa7d119901d8f343eba2b378a))
- **Mergify**: Configuration update (#107) ([a3c3c2a](https://github.com/matrixorigin/Memoria/commit/a3c3c2a901c6bb845666fa002306adbfcdc71d33))
- **Mergify**: Configuration update (#105) ([c43a73d](https://github.com/matrixorigin/Memoria/commit/c43a73d9d46c0e36c29540aa80c77da2c0153d29))

### 📚 Documentation

- Update README  (#146) ([cbccd42](https://github.com/matrixorigin/Memoria/commit/cbccd42a3d993656e213513c0155af9aef6f0af4))
- Prioritize Memoria Cloud, add Git-for-Memory tagline (#135) ([4b68e3b](https://github.com/matrixorigin/Memoria/commit/4b68e3be833e05f050b2a720982c7212a41646b1))
- **clawhub**: Rename to thememoria + update for api mode (#102) ([89a6500](https://github.com/matrixorigin/Memoria/commit/89a65000c87bc09adf55f2be6cb51fb56936a776))

### 📦 Miscellaneous

- Enhance connection pool health monitoring, Prometheus metrics, and load test (#158) ([bcd20ab](https://github.com/matrixorigin/Memoria/commit/bcd20ab599daa63233bfee353c117bee0b02a2e0))
- Add code owners (#147) ([10aff34](https://github.com/matrixorigin/Memoria/commit/10aff340d82f613cc08d282a3bda7ab1b49437e0))
- **plugin**: Bump to v0.4.1 for npm republish (#104) ([f6ee2cc](https://github.com/matrixorigin/Memoria/commit/f6ee2ccbf2a79deb097034a8dd5223c294cb099d))
- Cleanup python (#100) ([0333063](https://github.com/matrixorigin/Memoria/commit/0333063b003096a99ba7120ef9e19fee9823e206))

### 🚀 Features

- Auto-migrate legacy local startup (#178) ([0d71169](https://github.com/matrixorigin/Memoria/commit/0d711693744d6623cfef08d1aea0dc29c9d29652))
- Support trace binary install count (#174) ([885140a](https://github.com/matrixorigin/Memoria/commit/885140ae561a7f377912240b221d59aa4e10020b))
- Add autoApprove to generated MCP config (fixes #74) (#170) ([ceb97da](https://github.com/matrixorigin/Memoria/commit/ceb97da37f775042d1fcf13784849ec4ee52054d))
- **storage**: Implement per-user DB isolation and migration tooling (#167) ([ac1470a](https://github.com/matrixorigin/Memoria/commit/ac1470a5cf21d88ae739f64de303769f39f5efe9))
- Add user streamable http metrics (#145) ([24eda05](https://github.com/matrixorigin/Memoria/commit/24eda050e3b52a4366991a40ee071cb3101d86cf))
- Streamable http mcp server (#140) ([9b70130](https://github.com/matrixorigin/Memoria/commit/9b70130562b7c96181fb1772ed850489dc1cd869))
- Add Windows x86_64 binary build and PowerShell install script (#133) ([04ab5b1](https://github.com/matrixorigin/Memoria/commit/04ab5b1a7ae82f25442d8c6b181751e1791c4d7a))
- Embedding metrics, Grafana dashboard overhaul & monitoring stack (#137) ([597a431](https://github.com/matrixorigin/Memoria/commit/597a431c039c579c1e6a317402ac4d06b553a730))
- **openclaw**: Add X-Memoria-Tool header to API requests (#108) ([1d013ed](https://github.com/matrixorigin/Memoria/commit/1d013edcce5565a6f6db589130200db1cb7a876e))
- Add gemini cli support (#87) ([8e79ced](https://github.com/matrixorigin/Memoria/commit/8e79ceda4eb306316c9b896490a1c63359698aa1))
- **plugin**: Direct HTTP API mode + mem/content leak bug fix + rename to @matrixorigin/thememoria (#101) ([fe0e42c](https://github.com/matrixorigin/Memoria/commit/fe0e42c61e313f0db98c2cd781690cfc413c63fa))
- Support multi embedding endpoints (#97) ([06f9862](https://github.com/matrixorigin/Memoria/commit/06f98627233ba0c96dea08ebedbc76111d51aae4))
- Track per-user tool access times with in-memory cache and periodic DB flush (#94) ([bd18be6](https://github.com/matrixorigin/Memoria/commit/bd18be612b55307d2b4226bc3a78a331fded7266))
- **openclaw**: Cloud/local connect command + fail-safe onboarding (#70) ([3b73268](https://github.com/matrixorigin/Memoria/commit/3b732684f828fc902369023c55df8e5ceb96e098))
## [0.2.3] - 2026-03-21

### ⚡ Performance

- Fix performance issues (#73) ([b64de86](https://github.com/matrixorigin/Memoria/commit/b64de86360b0d795917df22985b9a1650e889191))

### 📚 Documentation

- **readme**: Polish beat card spacing and idea bulb icon (#69) ([c88cccc](https://github.com/matrixorigin/Memoria/commit/c88cccccfe9c07ccc6cb728d5731d71e310cd2e3))
- Add Git for Data story demo (#66) ([2d71edb](https://github.com/matrixorigin/Memoria/commit/2d71edbeed9d4112f66e719b6ef578607cadf8ba))

### 🚀 Features

- Add ClawHub Memoria skill bundle (#71) ([7b4034a](https://github.com/matrixorigin/Memoria/commit/7b4034a6e416d1bc92711f0e5227784d3c76e5d3))
- Distributed Deployment: Health Checks, Pool Metrics, OpenTelemetry, Grafana Dashboard (#67) ([cd77c9e](https://github.com/matrixorigin/Memoria/commit/cd77c9e350b452935044273ebfdae40d6500a7c9))

### 🧪 Testing

- Add more test (#68) ([50c3d9a](https://github.com/matrixorigin/Memoria/commit/50c3d9a4d19eb2ee0006ac0be5bbc57837568a84))
## [0.2.2] - 2026-03-20

### 🐛 Bug Fixes

- Security and snapshot count  (#61) ([eafed50](https://github.com/matrixorigin/Memoria/commit/eafed5001036ff6720281eea5a625b793bcfc036))
- Flaky ci test (#58) ([c788dc0](https://github.com/matrixorigin/Memoria/commit/c788dc072c3d6541d438d75a35fd8fa1d88a7887))

### 🚀 Features

- Feedback system, adaptive retrieval, governance audit trail, API hardening, and comprehensive e2e tests (#65) ([539fd8a](https://github.com/matrixorigin/Memoria/commit/539fd8a3aa33c8503400f8f824af697f1c59ac53))
- Add --tool flag to mcp subcommand and fix codex support (#64) ([1701094](https://github.com/matrixorigin/Memoria/commit/1701094a99fa7677d9e2264d1b92a3be6c67db1b))
- Interactive init prefill, Codex support, self-update, install auto-init (#63) ([6394d8d](https://github.com/matrixorigin/Memoria/commit/6394d8d5b5924a36adf8f5259bfaa436018ef3f2))
- Enable apikey authentication (#47) ([18c6c17](https://github.com/matrixorigin/Memoria/commit/18c6c17f1029244fd30149159a0c8e19142e57dd))
## [0.2.1] - 2026-03-19

### 🏗️ Build

- **ci**: Switch Linux release binaries to musl static linking (#56) ([b651cd1](https://github.com/matrixorigin/Memoria/commit/b651cd1abed188bca6b701e12be17000d8a35183))

### 📚 Documentation

- **openclaw**: Simplify install path and clarify success checks (#57) ([1e5a5f3](https://github.com/matrixorigin/Memoria/commit/1e5a5f3d350ccb26af3362da11533e97ea61823e))

### 🚀 Features

- More skills (#55) ([f62d777](https://github.com/matrixorigin/Memoria/commit/f62d777586e201956f5a86a89b15a476a0873561))

### 🧪 Testing

- Add session consistency test (#54) ([27bf9f8](https://github.com/matrixorigin/Memoria/commit/27bf9f89f5ac52eb6056c72fa0f30754a719cd9d))
## [0.2.0-rc] - 2026-03-19

### 🐛 Bug Fixes

- Update README logo to new memoria-logo asset (#52) ([7d1245f](https://github.com/matrixorigin/Memoria/commit/7d1245faa3e5b867016c91e0b3dd1363df8e8187))
- UTF-8 string truncation panic with multi-byte characters (#48) ([f774499](https://github.com/matrixorigin/Memoria/commit/f774499caa6c6b981d0a8eaa1b4f3d46d831e3ae))
- Install.sh Text file busy error when upgrading (#46) ([a79f971](https://github.com/matrixorigin/Memoria/commit/a79f97120b33ce1f91aef42518073fdc806be472))

### 🚀 Features

- **benchmark**: Separate official LongMemEval and BEAM reporting (#50) ([72998fe](https://github.com/matrixorigin/Memoria/commit/72998fe38676209c6a75de2da606998d8da26414))
- Add OpenClaw-native Memoria onboarding (#49) ([7560190](https://github.com/matrixorigin/Memoria/commit/75601906907079417f6756b86ee538d8085df14e))
- Implement plugin framework (#45) ([4301f97](https://github.com/matrixorigin/Memoria/commit/4301f976e0757b9d465b43891bd20745aea17a28))
- Replace hand-rolled prompts with cliclack TUI wizard (#44) ([ef071bd](https://github.com/matrixorigin/Memoria/commit/ef071bd33be262154fee23903edaa33fbe81d4b6))
## [0.1.0] - 2026-03-18

### 🐛 Bug Fixes

- Install.sh latest URL format (/releases/latest/download/ not /releases/download/latest/) ([ddbd06e](https://github.com/matrixorigin/Memoria/commit/ddbd06edc9885257ea2b70fa24d67db076e92351))
- Docker release username (#43) ([18ebca5](https://github.com/matrixorigin/Memoria/commit/18ebca503f4017ea81cd152b83e32afea17ec01d))

### 🚀 Features

- **cli**: Connectivity checks for DB and embedding in interactive init (#42) ([aedb529](https://github.com/matrixorigin/Memoria/commit/aedb52998949cfefb6cf6375130001f34da1dd71))
- Interactive setup wizard (memoria init -i) and improved install script (#41) ([e0df7b9](https://github.com/matrixorigin/Memoria/commit/e0df7b9959863a3fea2bff7f76063f5f89e9a02e))
## [0.1.0-rc2] - 2026-03-17

### 🐛 Bug Fixes

- Missing from rust refactor  (#39) ([fb38d6c](https://github.com/matrixorigin/Memoria/commit/fb38d6c535a363368a6fb17ac0ead7c9da65f8e8))
## [0.1.0-rc1] - 2026-03-17

### Sync

- Update to v0.2.5 - auto table creation, embedding dim switching ([0379674](https://github.com/matrixorigin/Memoria/commit/037967407571a76c5f5809af7583dcff49b392e8))

### ⚡ Performance

- Small opts (#27) ([2e9af19](https://github.com/matrixorigin/Memoria/commit/2e9af196d0cdefe6718b6f8d245cc9d5137ec734))
- Enhance explain (#26) ([cf60fce](https://github.com/matrixorigin/Memoria/commit/cf60fcec9c4447c636b4b1adc3d6eb6488436cb4))
- Improve memory retrieval and graph operations (#21) ([aa50616](https://github.com/matrixorigin/Memoria/commit/aa50616d7d31277f0b610e27abd656a84ec58fba))

### 🐛 Bug Fixes

- Cross toml ([f7d24ad](https://github.com/matrixorigin/Memoria/commit/f7d24ad0d725070ebf6298c54c200464a8cd6d7b))
- Cross compiling env (#38) ([3b35ac1](https://github.com/matrixorigin/Memoria/commit/3b35ac1e6ae732b5bb88ba6e271abe6c7a90f2e7))
- Release dep (#36) ([5c7b347](https://github.com/matrixorigin/Memoria/commit/5c7b3476992a58628191eb2e06140eeceb843c8c))
- Remove hardcoded version in memoria-git dependency (#35) ([5b142e6](https://github.com/matrixorigin/Memoria/commit/5b142e6e796e758734fa337cba9efa2e642b3d3d))
- Ci related (#34) ([e1b7e0c](https://github.com/matrixorigin/Memoria/commit/e1b7e0c911926e66df93ed3e6c6d113ba24906b1))
- Hybrid search (#30) ([e1f319f](https://github.com/matrixorigin/Memoria/commit/e1f319f3985e25c62f7316b06eab7aa9395c9c6f))
- Issue #22. (#23) ([068dda3](https://github.com/matrixorigin/Memoria/commit/068dda373ea13d741770f31ebbb9b661436775f8))
- Batch inject (#24) ([85c4675](https://github.com/matrixorigin/Memoria/commit/85c4675a95c45d4b1f53821ab964f6c6aa7bca98))
- Sync version to 0.1.14 and remove 500-char limit in rule version… (#20) ([ad84469](https://github.com/matrixorigin/Memoria/commit/ad8446936bb67c590c8f23543d9e9fe4cd60e6ed))
- Stale graph node retrieval and cooldown cache pollution (#15) ([b1dd0b3](https://github.com/matrixorigin/Memoria/commit/b1dd0b382e2597d1c8f333c3e9e8d94b82d58287))
- Resolve branch name hyphen bug, add input validation, optimize CI workflows (#8) ([cc82cf9](https://github.com/matrixorigin/Memoria/commit/cc82cf90cd7ddb97311dd4f42a65a534c0d277cb))

### 📚 Documentation

- Add CPU-only PyTorch installation guide for non-GPU environments (#12) ([8e1af3a](https://github.com/matrixorigin/Memoria/commit/8e1af3a6b64d44e9e3c846ad0b4073663388e26d))
- Guide users to edit config files after init if needed ([734f5ed](https://github.com/matrixorigin/Memoria/commit/734f5ed3c3b022d0cb96c41699a64c66e74a8240))
- Emphasize embedding config is irreversible - add critical warnings ([31971e3](https://github.com/matrixorigin/Memoria/commit/31971e3d4e4c31100871d3ec9268749e43160c42))
- Emphasize guided config with all env vars always present ([bee7f7d](https://github.com/matrixorigin/Memoria/commit/bee7f7d1f4934dd51e31ae73779ce5c82a49e4b0))

### 📦 Miscellaneous

- Remove openssl-sys ([598bd66](https://github.com/matrixorigin/Memoria/commit/598bd66bcbc3382a1c9fef1a53450d88ebda0d33))
- Consolidate test commands and update package name to mo-memoria (#7) ([7d94c6c](https://github.com/matrixorigin/Memoria/commit/7d94c6c1bde25c3bc3b4347c624a6c178e4dc52b))
- Add mergify (#4) ([b71f0fe](https://github.com/matrixorigin/Memoria/commit/b71f0fe7c0a5a559152aad4115bd9f30d3fce0d6))

### 🔧 Refactoring

- Refactor all with rust (#33) ([62a7076](https://github.com/matrixorigin/Memoria/commit/62a7076b513925b382db07051d021aba5b6060c4))

### 🚀 Features

- Add installer script and improve release workflow (#37) ([32fae53](https://github.com/matrixorigin/Memoria/commit/32fae53d1ae25eda373d994c3dc51d5737833459))
- Lots of changes to api server and bug fixes (#29) ([1b3d479](https://github.com/matrixorigin/Memoria/commit/1b3d47980522dd7d086937379fefef1852c8ea64))
- Enhance with explain (#25) ([07fed35](https://github.com/matrixorigin/Memoria/commit/07fed357a2a8e7cf4309d6a8fd003c766835e147))
- Optimize MCP server startup and database configuration loading (#16) ([e908548](https://github.com/matrixorigin/Memoria/commit/e90854805ddcde4a1326d2560f6b3d7f582f75a0))
- Enable offline mode for local embedding by default (#3) ([d446a13](https://github.com/matrixorigin/Memoria/commit/d446a1309f44de5b8b9d50a245876baefd88bee1))
- L0/L1 tiered memory retrieval + redundancy compression + governance improvements (#2) ([9d99085](https://github.com/matrixorigin/Memoria/commit/9d9908572cf6d758dafff6d92a8cb8373b97d499))

