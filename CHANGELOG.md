# Changelog

## [1.1.0](https://github.com/bestlux/boundless/compare/v1.0.0...v1.1.0) (2026-02-21)


### Features

* **input:** harden injection runtime and ownership gating ([7c5a9e6](https://github.com/bestlux/boundless/commit/7c5a9e6ceb1c78293e6e0896066ac1bf470f5d88))
* **network:** harden framing, flushing, and inbound file handling ([1698d2d](https://github.com/bestlux/boundless/commit/1698d2da0103b534e6d99946dc532dbfed1d0474))


### Performance Improvements

* improve performance + tray UI ([#3](https://github.com/bestlux/boundless/issues/3)) ([71db535](https://github.com/bestlux/boundless/commit/71db5356d549a011204c4d6ff72ae2cea7f71028))

## 1.0.0 (2026-02-18)


### Features

* add approval-based nearby pairing flow ([648e4fa](https://github.com/bestlux/boundless/commit/648e4faf5647c7d107c6e136df1dfb687e3f438d))
* add clipboard image sync runtime and transport hooks ([10a0ba2](https://github.com/bestlux/boundless/commit/10a0ba280d6acafd65dce9a22cd176af65c3840d))
* add diagnostics hotkey action triggers ([37319fd](https://github.com/bestlux/boundless/commit/37319fdc82e9958b41de3f501a4f19d2239192c6))
* add input capture target runtime and windows capture polling ([dfd12f2](https://github.com/bestlux/boundless/commit/dfd12f2b7e9c8514920f67e18abf01f672539033))
* add input latency telemetry and report tooling ([bf88082](https://github.com/bestlux/boundless/commit/bf88082a1d39766b3eef6ad433a479f62127806f))
* add input ownership control plane and routing primitives ([822e7d1](https://github.com/bestlux/boundless/commit/822e7d1c11505ed1c89f8f643633ef4a1e2eea5a))
* add input runtime queue and synthetic key helper ([9465f03](https://github.com/bestlux/boundless/commit/9465f034262bbe99009ef704f809b679c7ae606a))
* add interactive console flow for runtime control ([3b57301](https://github.com/bestlux/boundless/commit/3b573019b755b0ba31d4faef86d5286aa9939e96))
* add layout-driven edge capture handoff ([6a52b9e](https://github.com/bestlux/boundless/commit/6a52b9ead3675f2b2fc857517adbe6c4e3183db7))
* add mDNS discovery runtime with manual fallback ([e68265c](https://github.com/bestlux/boundless/commit/e68265c3ec0de348efe5e49d91df1da943b6cb35))
* add payload transfer commands and transport diagnostics ([0ae85da](https://github.com/bestlux/boundless/commit/0ae85daaf65fea49e12d536675b3e01ca54d207d))
* add runtime clipboard text sync pipeline ([ec54ff2](https://github.com/bestlux/boundless/commit/ec54ff2b067670420a9bde2674d23f07bb2e06dc))
* add tls peer transport, trust bundles, and two-node smoke harness ([fb1fa80](https://github.com/bestlux/boundless/commit/fb1fa80ce16c6d99e30ac4096bd176105edcf42e))
* add windows hotkey runtime actions ([88c78fc](https://github.com/bestlux/boundless/commit/88c78fcc796e6a1bad48b4624d9d517e625e75a8))
* add windows low-level hook capture backend ([4ee4177](https://github.com/bestlux/boundless/commit/4ee417744b67eff39dea7ac20366a511bb147e6f))
* add windows named-pipe control plane transport ([47ea4cb](https://github.com/bestlux/boundless/commit/47ea4cbd3a92939732423b5f534ebcc41cafc4f7))
* bootstrap boundless workspace with daemon, cli, and CI/release automation ([f41924a](https://github.com/bestlux/boundless/commit/f41924a8ab18f6423cc2d37b4d2f8c20b91d6687))
* **daemon:** add strict input lock and topology handoff runtime ([a48cf32](https://github.com/bestlux/boundless/commit/a48cf325b4335d29449484649532c66301841c75))
* **dev:** add edge handoff trace capture helper ([0c8b470](https://github.com/bestlux/boundless/commit/0c8b470a13b49fed21ab0bf71261662a3d456651))
* **dev:** add unified test suite runner profiles ([26560c5](https://github.com/bestlux/boundless/commit/26560c582eb10f433a87f3eb70d47c67b43f559c))
* implement switch-all hotkey capture rotation ([d5ce094](https://github.com/bestlux/boundless/commit/d5ce094b05856c8b1d2e704b5fc573228859f8c1))
* implement windows sendinput input injection backend ([a7bfa31](https://github.com/bestlux/boundless/commit/a7bfa318001ba6329efb00ec7ae1122ab48bc81f))
* **input:** add raw-input mouse capture with hook fallback ([8f60106](https://github.com/bestlux/boundless/commit/8f6010658a7d012cef32886ccda45dbb36dc45b0))
* **input:** auto-start control on local edge handoff ([b35917f](https://github.com/bestlux/boundless/commit/b35917fa40fb0426c0d8d93f8e2c2fe3d9fbd756))
* route input frames over transport with noop sink ([6d4c96b](https://github.com/bestlux/boundless/commit/6d4c96bae12167b36c1b6e828410d93665870a40))
* simplify console pairing with discovered peer requests ([c34b70f](https://github.com/bestlux/boundless/commit/c34b70f73f74b2a89ebf63fa7f6814ff08e95368))


### Bug Fixes

* accept hostname bundle addresses and harden pairing port fallback ([37918d1](https://github.com/bestlux/boundless/commit/37918d19ff5d92e354bc2e6f14ae94a80bb6d005))
* auto-fallback transport bind port on startup ([0acdc4a](https://github.com/bestlux/boundless/commit/0acdc4a39f871154dac57fe5e364542966048fc6))
* avoid input replay after partial sendinput success ([94bba9d](https://github.com/bestlux/boundless/commit/94bba9d0161b5dfa6ef17721be3744a23e375124))
* avoid non-windows pipe-path unused warning ([0d03bd5](https://github.com/bestlux/boundless/commit/0d03bd5e1379404499fb013a7c7cdeddf843c8ea))
* clear peer input sequence state on disconnect ([a1dbdb5](https://github.com/bestlux/boundless/commit/a1dbdb553559172864c879a511115631dcebd45d))
* clear stale input owner when removing peer ([6cd741d](https://github.com/bestlux/boundless/commit/6cd741d80b799fcd67387b465244bbfb2245627b))
* **daemon:** forward held-key repeats during remote control ([1213948](https://github.com/bestlux/boundless/commit/121394854bd18468b6d3d86a5217737333cc3c83))
* **daemon:** gate hotkey warn import for non-windows builds ([a8ffe64](https://github.com/bestlux/boundless/commit/a8ffe646314fcdc9413c6adf8f89220178be5356))
* **daemon:** gate windows-only hotkey/input items ([403cfc6](https://github.com/bestlux/boundless/commit/403cfc640fd8d6fbdf3c311a9432ba21a914977c))
* **daemon:** make edge handoff work with single-peer legacy layouts ([6723732](https://github.com/bestlux/boundless/commit/6723732b0968f982fe49be6a49b703878a29f716))
* **daemon:** reduce edge handoff bounce and anchor drift ([91fe27b](https://github.com/bestlux/boundless/commit/91fe27b113c024539cf15ab4e3db7769a0217858))
* **daemon:** resolve cross-platform clippy warnings ([e22eb60](https://github.com/bestlux/boundless/commit/e22eb60a013b3715de3d4fed0cca009efec3f9b4))
* default config_version for legacy config parsing ([81720d4](https://github.com/bestlux/boundless/commit/81720d47020d89588c8d8be828d8b49b0b2fea16))
* **dev:** avoid stale last exit code in test suite runner ([2d7d25a](https://github.com/bestlux/boundless/commit/2d7d25a24711d3efec655a81a9273e7d42e13094))
* drain capture hook events while target is inactive ([8e2ab09](https://github.com/bestlux/boundless/commit/8e2ab095a803e5801c64e76599cd795b7a54fd9e))
* enforce pairing code validation and bind precheck ([b091891](https://github.com/bestlux/boundless/commit/b091891c478177c5e40cf9dceb63a2bb304d4380))
* fail smoke scripts when cargo build exits non-zero ([405e1fd](https://github.com/bestlux/boundless/commit/405e1fdf7967583585b4003e1369d2f4b970c1b6))
* flush input release events on capture target transition ([745bb2c](https://github.com/bestlux/boundless/commit/745bb2c19b668d9fc73dfc4005596f230489e70f))
* force reconnect hotkey to tear down active sessions ([f24f14d](https://github.com/bestlux/boundless/commit/f24f14de3186d3009ed49adbca3a7f1124bf9c4c))
* gate image frames by protocol and validate bmp payloads ([0600974](https://github.com/bestlux/boundless/commit/06009740422cc95838c4228d0f6adea60dc90a6b))
* gate windows-only input runtime symbols ([eb2c77b](https://github.com/bestlux/boundless/commit/eb2c77b781694d5173ce5dea17716908f20dc2f1))
* hard-abort reconnect sessions and preserve hotkey edges ([c79cfc9](https://github.com/bestlux/boundless/commit/c79cfc9f028ac20cd1674de70e957d7c549db70e))
* harden peer identity and trust import handling ([fc33fce](https://github.com/bestlux/boundless/commit/fc33fceb8fca6f587ecdbb913eeca357aa35ad46))
* **input:** anchor cursor position on edge handoff ([8ed05c5](https://github.com/bestlux/boundless/commit/8ed05c5783fd4c32eeb1edbf171f10df93330d7d))
* **input:** gate auto handoff on real screen edges ([b1ff95b](https://github.com/bestlux/boundless/commit/b1ff95b953def13a25f091d3ada073ddf0610914))
* **input:** preserve move ordering and ignore absolute raw packets ([90d65ff](https://github.com/bestlux/boundless/commit/90d65ff04c1396e60b944ace60e497182bfa8526))
* **input:** restore local edge handoff in raw mode ([1e18d31](https://github.com/bestlux/boundless/commit/1e18d31174e665e1277beaed5705f14ec0466f3c))
* **input:** stabilize handoff routing and escape unlock ([913b087](https://github.com/bestlux/boundless/commit/913b087292c273ae53f0caba240ca94be5374bcf))
* **input:** suppress mouse move while lock is active ([c1e50fe](https://github.com/bestlux/boundless/commit/c1e50fe7929e2c27dd218d2b154f995a383a09de))
* **input:** trigger local handoff on edge push ([814c21d](https://github.com/bestlux/boundless/commit/814c21d0f6f8a18ecf0c9bb65303b20d1d7c5680))
* preserve mdns ipv6 scope and avoid partial trust imports ([7dc6718](https://github.com/bestlux/boundless/commit/7dc6718388e50a4ab98c6a0b5a67bf7298dcb55b))
* prevent stale clipboard dedupe while disconnected ([02a0c0d](https://github.com/bestlux/boundless/commit/02a0c0d4224c300345ae0b93297b748b617a740c))
* refactor input timing telemetry helpers for clippy ([703acd1](https://github.com/bestlux/boundless/commit/703acd18160ac06b9336e441d6a1a1c4197b0004))
* require active capture for edge handoff ([978a74a](https://github.com/bestlux/boundless/commit/978a74ac3a719504e3918f3e56f4783b044306a7))
* restore console control with inbound owner auto-claim ([c3c585f](https://github.com/bestlux/boundless/commit/c3c585f62dc6d87d1410f2b5d1e63101b02b16bc))
* retain unsent payloads and sanitize inbound file names ([b92c5a2](https://github.com/bestlux/boundless/commit/b92c5a2f102ff92a8b76f66743a12e0c1b7da5f1))
* retry busy named-pipe connects and report effective transport ([cd5e3dc](https://github.com/bestlux/boundless/commit/cd5e3dc9c24e5254a3f8f0654c850344ca96acc4))
* revalidate input ownership before queued inject ([2539d8e](https://github.com/bestlux/boundless/commit/2539d8eb5403a189698b03725ee6afc02670dd9a))
* scope smoke incremental override to guarded block ([0aefde6](https://github.com/bestlux/boundless/commit/0aefde6f1e72b078df7a5d1217dc55751520c8a9))


### Performance Improvements

* reduce input latency with fast outbound flush tick ([500d9f2](https://github.com/bestlux/boundless/commit/500d9f287adf13409876c756a9c91ab80d1c6ec0))
