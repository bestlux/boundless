# Changelog

## [5.0.14](https://github.com/bestlux/boundless/compare/v5.0.13...v5.0.14) (2026-07-11)


### Bug Fixes

* **ci:** inspect inherited kernel ACL rules ([b8c9086](https://github.com/bestlux/boundless/commit/b8c9086b9afc93a088ecc09dda5856c6d6608bf5))
* **ci:** make kernel ACL probe token-aware ([e929a93](https://github.com/bestlux/boundless/commit/e929a93691fb5dc495420fb801ca5474304c8734))
* **ci:** validate mutex dacl semantically ([4390cdd](https://github.com/bestlux/boundless/commit/4390cdd01b5dca35fa6bdbaf924fdeb12799069f))
* harden Windows input and upgrade lifecycle ([6eef42c](https://github.com/bestlux/boundless/commit/6eef42c8fbcd9a8ddf3125ac66ff8ead34024266))
* **input:** detect emergency unlock with raw keyboard ([57dd527](https://github.com/bestlux/boundless/commit/57dd5273ffb26845684a17001e73a1473d652ea1))
* **input:** fail open on broker replacement ([baf6a62](https://github.com/bestlux/boundless/commit/baf6a6267c47c196f70439ccfe6844a74480af2e))
* **input:** harden broker authority recovery ([f80f808](https://github.com/bestlux/boundless/commit/f80f8084859667664a7f8c8d7aa5a13b0055ed04))
* **input:** harden broker delivery and safety state ([ac61ad1](https://github.com/bestlux/boundless/commit/ac61ad1b02481a87366ec989e750ce75ba21fb26))
* **input:** harden emergency unlock detector ([f180a81](https://github.com/bestlux/boundless/commit/f180a8138cd470d01639cb8453740fd36eb34e0a))
* **input:** harden fail-open broker handshake ([cfc7161](https://github.com/bestlux/boundless/commit/cfc7161efdd07927da6b6bb4eca54afbd40ada19))
* **input:** harden num lock injection recovery ([049eafa](https://github.com/bestlux/boundless/commit/049eafa1d46887def4a76d5becbb0c0c91dbdd08))
* **input:** persist broker delivery receipts across reattach ([fe4e05a](https://github.com/bestlux/boundless/commit/fe4e05a0dfe5b0de294222c48b9cec50e162f06c))
* **input:** persist num lock state across frames ([ffb4a51](https://github.com/bestlux/boundless/commit/ffb4a51bb8da8f733663b44f8553e151ac51bc8a))
* **input:** preserve fail-open reconciliation ([7207252](https://github.com/bestlux/boundless/commit/72072522f44f8fb94f6d92debaf908a8233b704f))
* **input:** preserve held state across broker recovery ([9d4a53d](https://github.com/bestlux/boundless/commit/9d4a53daf98084b1c289310a2b617fab1dea8528))
* **input:** preserve numpad semantics across peers ([c7b346a](https://github.com/bestlux/boundless/commit/c7b346aa56de248ed1ec67cb87e20f8f9dba6c33))
* **input:** preserve safety unlock across broker sessions ([28d3936](https://github.com/bestlux/boundless/commit/28d39361eb659dc11e7e663516ff43c9b331c909))
* **input:** project keypad modifiers across num lock state ([92967fe](https://github.com/bestlux/boundless/commit/92967fedf53c850bde168ed2efe5c6ecbdf39e2f))
* **input:** restore held state before partial reattach retry ([b47a5af](https://github.com/bestlux/boundless/commit/b47a5aff1f4205b6ea0e18e0d48960de4c12e33c))
* **input:** revalidate broker retries before injection ([41b437c](https://github.com/bestlux/boundless/commit/41b437c4996ac06e532a546abfac4695b1a5633a))
* **input:** separate capture state and retry progress ([e086e5c](https://github.com/bestlux/boundless/commit/e086e5c7ac0a35d018e0ce276c39e23b892c7bbf))
* **installer:** bound deferred service recovery ([d1f7bd6](https://github.com/bestlux/boundless/commit/d1f7bd607a50b06af630229ff77f1783f3284690))
* **installer:** bound hard-kill recovery ([b47f2f0](https://github.com/bestlux/boundless/commit/b47f2f09907807e6692a0a22ccaa3ef8823dadee))
* **installer:** close integration security gaps ([64f4700](https://github.com/bestlux/boundless/commit/64f4700876ccb3b87aa370eb2b81f9373465453b))
* **installer:** close upgrade hardening gaps ([36cbb60](https://github.com/bestlux/boundless/commit/36cbb6018890158a908be40db685db28ab97c6f1))
* **installer:** fence privileged service recovery ([f2781ec](https://github.com/bestlux/boundless/commit/f2781ecc36560906d7403d902fdefa1c8e20dbd4))
* **installer:** harden elevated upgrade handoff ([0d79580](https://github.com/bestlux/boundless/commit/0d7958068235c02f97f2ad5f28efe53fd5e2ca21))
* **installer:** prevent upgrade shutdown loops ([5a0be2b](https://github.com/bestlux/boundless/commit/5a0be2bbd05967324d21f84a609358c535a1a23d))
* **installer:** recover after forced cancellation ([08e49bc](https://github.com/bestlux/boundless/commit/08e49bcddb39bd29f9f4b26541f7a78a2a4748c9))
* **installer:** restore service after preflight failure ([681424f](https://github.com/bestlux/boundless/commit/681424fd8bad44472568ef230f3662341bae9204))
* **installer:** retain quiescence through uncertain transactions ([1284adc](https://github.com/bestlux/boundless/commit/1284adc11cc0f0be760566d89a14395643ac67b8))
* **installer:** revoke privileged recovery children ([852cc7a](https://github.com/bestlux/boundless/commit/852cc7ada9d89d88f5a02e3814a779568a02ac9d))
* **installer:** secure upgrade quiescence boundary ([bc0c69d](https://github.com/bestlux/boundless/commit/bc0c69daa6f2757760d27a2847067acc94ce47cf))
* **installer:** supervise upgrade process trees ([cdd3c35](https://github.com/bestlux/boundless/commit/cdd3c35b9c0fa5749f5a6dfeba95d9ece0bc911a))
* **network:** bound startup replay and session egress ([2b766ee](https://github.com/bestlux/boundless/commit/2b766eea337d99789c4a61e62e138af2a99fa59e))
* **network:** converge competing transport sessions ([6b0aa59](https://github.com/bestlux/boundless/commit/6b0aa59c261c3c5351915d0755d6b2083a2d2de4))
* **network:** make clipboard replay latest wins ([b1d15a1](https://github.com/bestlux/boundless/commit/b1d15a11ddf3b7c980900eb1314c3a204e551640))
* **network:** preserve active session payloads ([cb48c2b](https://github.com/bestlux/boundless/commit/cb48c2b8e989cee6c6b3f3a1b7e16269fd2abac7))
* **test:** add guarded lock key semantics ([a59c16d](https://github.com/bestlux/boundless/commit/a59c16d234b0a389bc2a140f54f23c4919a90796))
* **test:** use keyboard state for num lock injection ([b50f206](https://github.com/bestlux/boundless/commit/b50f206b3948a691a2230b5dd2d3fd63602ba7d4))
* **windows:** close input and tray handoff races ([c6d6f59](https://github.com/bestlux/boundless/commit/c6d6f596f5c3a7659a345fe1a2217775480a2e27))
* **windows:** preserve keypad and tray handoff identity ([e545de7](https://github.com/bestlux/boundless/commit/e545de7aa31c4738775e729152c8743940d93415))

## [5.0.13](https://github.com/bestlux/boundless/compare/v5.0.12...v5.0.13) (2026-07-10)


### Bug Fixes

* **ci:** gate Windows-only lock lease ([123e1ab](https://github.com/bestlux/boundless/commit/123e1aba9b649e8f161d271e57f80c3b4e290376))
* **clipboard:** isolate broker failures from input ([0235d25](https://github.com/bestlux/boundless/commit/0235d2508f45d1af3e7d53476ed4b9755fdb8e27))
* **clipboard:** order brokered local updates ([a6d76f4](https://github.com/bestlux/boundless/commit/a6d76f4a6bbd4d6f05712356e260be13ac4d07c9))
* **clipboard:** preserve newest broker payload ([4593fea](https://github.com/bestlux/boundless/commit/4593fea1a9e9a77252dc922aaf27ffbedd5b3386))
* **clipboard:** retry transient broker reads ([106d289](https://github.com/bestlux/boundless/commit/106d2896e9d8fcf096a593fd226692293e778dd1))
* **diagnostics:** bound failure aggregates accurately ([b3c64d9](https://github.com/bestlux/boundless/commit/b3c64d924609a6372364077b34d2b3dc143d6579))
* **diagnostics:** canonicalize activity aggregation ([77ec135](https://github.com/bestlux/boundless/commit/77ec1356be61d389536b58f7972d293e4f6ace7c))
* **diagnostics:** harden failure retention and clipboard telemetry ([b1f38a1](https://github.com/bestlux/boundless/commit/b1f38a1ff5414554f942aa7dc00c46ab75505c3b))
* **diagnostics:** preserve causal runtime events ([e3f1752](https://github.com/bestlux/boundless/commit/e3f1752b3ab4a102c4d3853a06875a871e671f11))
* **input:** capture raw touchpad wheel deltas ([06facb7](https://github.com/bestlux/boundless/commit/06facb70cdc8f7dae1147c87df7e9844ff6fa3e6))
* **input:** close broker shutdown races ([28d66dc](https://github.com/bestlux/boundless/commit/28d66dc8a0ef123b2b2194656d751b0a36773d42))
* **input:** fail open direct capture locks ([10ef0ed](https://github.com/bestlux/boundless/commit/10ef0ed2a8017c9ce7d3788d1624d612e47bd7c0))
* **input:** harden broker capture lifecycle ([6f1bd28](https://github.com/bestlux/boundless/commit/6f1bd2828e2730c19d5a52d7ec061ad7d1126044))
* **input:** make emergency unlock fail open ([be32484](https://github.com/bestlux/boundless/commit/be32484b79e9a2ca84c40752d2ca7e55c4a9797b))
* **input:** preserve releases when broker replacement defers ([3c43e79](https://github.com/bestlux/boundless/commit/3c43e792a9d53fa94d1e932388405b1beedf68d3))
* **input:** preserve stale broker cleanup identity ([68f42f1](https://github.com/bestlux/boundless/commit/68f42f18affb6ce2bce6d565323e6f8ed0edba79))
* **input:** release held state on broker replacement ([a13be17](https://github.com/bestlux/boundless/commit/a13be17d86d6786f759fc10f789cde2b0cec4c5b))
* **input:** suppress late wheel source duplicates ([dfeba9c](https://github.com/bestlux/boundless/commit/dfeba9cc79ce8b2fd8ebf9f8b2677980186fc55c))
* **input:** widen emergency gesture timing ([6cdb326](https://github.com/bestlux/boundless/commit/6cdb32656d77978624e9d24471574cfa6555e109))
* **installer:** reject stale runtime verification ([7a39d66](https://github.com/bestlux/boundless/commit/7a39d66311e3e35f997c6e5b3d6a66a6ae141b74))
* **installer:** verify packaged installation ([5dc1521](https://github.com/bestlux/boundless/commit/5dc152178ceb829a2cfcb16ab2698926d4e09511))
* **privacy:** redact clipboard image rejection hashes ([6115c6b](https://github.com/bestlux/boundless/commit/6115c6b47d2828fe38cd8ffd93b9c59562d90b47))
* **privacy:** remove clipboard content from runtime events ([42a31ad](https://github.com/bestlux/boundless/commit/42a31adaf26659c3c75e0fb34f7c4f2bdcfa55ab))
* **smoke:** validate aggregated redacted events ([25da249](https://github.com/bestlux/boundless/commit/25da24960138d5b1b6dd5f8ca932e00a97b2c01f))
* **tray:** recover stopped Windows service ([d516728](https://github.com/bestlux/boundless/commit/d516728c669cdc6e4ea6cd3bd5ec306df014adab))
* **tray:** shut down input broker safely ([dacfad8](https://github.com/bestlux/boundless/commit/dacfad87d7cce7efaa4770f42f99a3f883f6873e))

## [5.0.12](https://github.com/bestlux/boundless/compare/v5.0.11...v5.0.12) (2026-07-09)


### Bug Fixes

* enforce one Boundless tray dashboard per Windows user session and activate the existing dashboard on subsequent launches
* preserve single-instance behavior when upgrading from the legacy 5.0.11 tray and recover ownership after a crashed tray process

### CI

* validate duplicate tray launches during interactive Windows installer smoke
* keep hosted Windows Clippy compatible with explicit egui stroke-width types

## [5.0.11](https://github.com/bestlux/boundless/compare/v5.0.0...v5.0.11) (2026-07-08)


### Bug Fixes

* broker clipboard access through the user-session tray broker when the daemon runs as a Windows service, so service-mode copy/paste reaches the interactive desktop clipboard
* honor Windows SCM stop requests promptly and abort daemon runtime work instead of wedging MSI install or upgrade sessions
* keep transport diagnostics readable by dropping periodic safety-tick wake events from the transport event ring and adding boundlessctl transport events --kind/--exclude-kind
* repair reset and connectivity helper paths used by two-PC dogfood smoke, including daemon-status parser coverage and packaging-script CI self-tests
* preserve asymmetric LAN pairing and trusted transport behavior for one-sided reachability dogfood scenarios

### CI

* run Windows packaging-script self-tests and the CLI daemon-status output contract in CI and release validation

## [5.0.0](https://github.com/bestlux/boundless/compare/v4.0.2...v5.0.0) (2026-06-19)


### Features

* add cluster anti-idle power requests ([3158326](https://github.com/bestlux/boundless/commit/31583269dc1f4004192a53faf5183c5a5b1c1b3c))
* add cluster anti-idle power requests ([3158326](https://github.com/bestlux/boundless/commit/31583269dc1f4004192a53faf5183c5a5b1c1b3c))
* add cluster anti-idle power requests ([d6f00db](https://github.com/bestlux/boundless/commit/d6f00db22d3cf3e707c854e5ad729f0ce3882f84))
* add configurable file receive flow ([a11ad0e](https://github.com/bestlux/boundless/commit/a11ad0e7a49a877dcc950fd6a7696a10ef4bb87c))
* add configurable file receive flow ([a23fb18](https://github.com/bestlux/boundless/commit/a23fb18f2386c62850bf71685fe20f6eb14c5261))
* add guarded windows service host ([2bf5f57](https://github.com/bestlux/boundless/commit/2bf5f57a011afac59b17983f160075050de205fa))
* add reliability health diagnostics ([c0eba8c](https://github.com/bestlux/boundless/commit/c0eba8c3e6e5b5e2938d70f7e80e60f45952f3a8))
* add trust rotation recovery path ([1387e14](https://github.com/bestlux/boundless/commit/1387e14dae7e45d658605d968b40fb23286c249b))
* add v5 readiness packet gate ([75c6d32](https://github.com/bestlux/boundless/commit/75c6d32c8cbea9e1a4905aa61cd34dca96476c97))
* complete tray settings surface ([6f7e46e](https://github.com/bestlux/boundless/commit/6f7e46eba670bceebc03714e2f71742df200e308))
* enforce v5 topology layout contract ([30391d2](https://github.com/bestlux/boundless/commit/30391d2b6409867b52552092a13841af87fb2e5d))
* expose input handoff policy ([7b382c4](https://github.com/bestlux/boundless/commit/7b382c452a7c0805f31b3e950661c6163f57ed12))
* harden file transfer workflows ([5c757ce](https://github.com/bestlux/boundless/commit/5c757ce9dcb3fd4dab2c78ce1c05d8bad806c7af))
* harden v5 release packaging ([5bfa5b3](https://github.com/bestlux/boundless/commit/5bfa5b3e9aef9a371e032e7fcc13b02f7de7cb64))
* harden windows service mode ([6b9ec75](https://github.com/bestlux/boundless/commit/6b9ec75e322c1ea20e24e3e97b7fb77f9ac1fd18))
* prepare Boundless v5 release ([ccb5b8c](https://github.com/bestlux/boundless/commit/ccb5b8c767bebba82be5a8da1e321bf0bd795d75))


### Bug Fixes

* align anti-idle tests with platform support ([06dac7d](https://github.com/bestlux/boundless/commit/06dac7da3aebab455dadc9b8b3d5e503f31e9cea))
* compile hotkey validation on Linux ([0be57ca](https://github.com/bestlux/boundless/commit/0be57cafef600173fdc0dfcc9f666058e520c063))
* gate Windows-only CLI service symbols ([ccf2b17](https://github.com/bestlux/boundless/commit/ccf2b17a5878f93b6fd91d0eddf05809e9f17186))
* recover stale named pipe from cli startup ([d6ee7c3](https://github.com/bestlux/boundless/commit/d6ee7c31c73c7837cda872dd8870f0354fe9737a))
* recover stale named pipe from cli startup ([00b80f6](https://github.com/bestlux/boundless/commit/00b80f6a00889453a8dc830c9cfc76bf487d8200))
* refresh layout on peer changes ([fea04de](https://github.com/bestlux/boundless/commit/fea04de2341a54bafd5662887a2f93570feae80a))
* satisfy anti-idle clippy lint ([b016f3d](https://github.com/bestlux/boundless/commit/b016f3d8400e79c37d70c1f4d12c5fda655739ce))
* satisfy current clippy lints ([30b9d6b](https://github.com/bestlux/boundless/commit/30b9d6ba615934ae93a2e484f40f90e2e77c0032))
* **tray:** move layout tests after helpers ([bf3b33a](https://github.com/bestlux/boundless/commit/bf3b33a897e48bc4fd7249da88ad121bc12938c6))
* **tray:** refresh layout on paired peer changes ([fea04de](https://github.com/bestlux/boundless/commit/fea04de2341a54bafd5662887a2f93570feae80a))
* **tray:** refresh layout on paired peer changes ([eed49e2](https://github.com/bestlux/boundless/commit/eed49e2d881e965bc14bbd859f4f1177ed14d340))

## [4.0.2](https://github.com/bestlux/boundless/compare/v4.0.1...v4.0.2) (2026-04-13)


### Bug Fixes

* **release:** accept empty install roots after uninstall ([99095e6](https://github.com/bestlux/boundless/commit/99095e6f3ad06ef89cfdb920ea79c1495b84651c))

## [4.0.1](https://github.com/bestlux/boundless/compare/v4.0.0...v4.0.1) (2026-04-13)


### Bug Fixes

* **windows:** harden installer upgrade recovery ([43540c1](https://github.com/bestlux/boundless/commit/43540c1e972679dd6a46aa950989a4eef68786d4))

## [4.0.0](https://github.com/bestlux/boundless/compare/v3.0.0...v4.0.0) (2026-04-12)


### ⚠ BREAKING CHANGES

* **tray:** tray dashboard interaction flow, layout management behavior, and pairing feedback semantics are reset for the 4.0.0 UX baseline.

### Features

* **tray:** redesign dashboard pairing and layout flows ([d7e1143](https://github.com/bestlux/boundless/commit/d7e1143a561472d7f7a193b5e9c469a1804bfcb4))


### Bug Fixes

* restore cross-platform daemon input result import ([1021927](https://github.com/bestlux/boundless/commit/102192774b135a6b1f0b7482c7f05a6cf610377a))

## [3.0.0](https://github.com/bestlux/boundless/compare/v2.1.0...v3.0.0) (2026-03-31)


### Bug Fixes

* **ci:** repair tray eframe upgrade on windows ([9cb0c05](https://github.com/bestlux/boundless/commit/9cb0c055cd98cf7a415b1684c79d3e69089aadbe))
* **ci:** repair tray eframe upgrade on windows ([e9ba389](https://github.com/bestlux/boundless/commit/e9ba389cf005a031f9e7d7c48de7689a3c72d065))
* **release:** accept installer-managed shortcut icons ([#26](https://github.com/bestlux/boundless/issues/26)) ([98945c2](https://github.com/bestlux/boundless/commit/98945c218f664cbdf1ade70fef893daed87d206a))
* **release:** coerce pwsh switch inputs ([#24](https://github.com/bestlux/boundless/issues/24)) ([a04b277](https://github.com/bestlux/boundless/commit/a04b277a44efa27c9436bef999042736e993f35c))
* **release:** grant publish job pull request permissions ([#34](https://github.com/bestlux/boundless/issues/34)) ([cec0fdb](https://github.com/bestlux/boundless/commit/cec0fdb3c2b41739a2bf51be7193c4e6c3d8cf71))
* **release:** guard uninstall registry lookup ([#27](https://github.com/bestlux/boundless/issues/27)) ([cbb93d1](https://github.com/bestlux/boundless/commit/cbb93d18393fd315e3dda77347cb7f7e428dee5a))
* **release:** make windows signing optional ([#25](https://github.com/bestlux/boundless/issues/25)) ([4c9b465](https://github.com/bestlux/boundless/commit/4c9b46513c4a94202ed1150c48e249cf5461aa0b))
* **release:** recover tagged publish flow ([#22](https://github.com/bestlux/boundless/issues/22)) ([09c3879](https://github.com/bestlux/boundless/commit/09c3879b52159b1d6328c570ab84933db3c1f7bc))
* **release:** recreate draft releases without tag cleanup ([#32](https://github.com/bestlux/boundless/issues/32)) ([112d127](https://github.com/bestlux/boundless/commit/112d1277f9098a4866774a77b759f59edc1ad850))
* **release:** set gh repo context for publish ([#31](https://github.com/bestlux/boundless/issues/31)) ([6f64412](https://github.com/bestlux/boundless/commit/6f64412cbe579df1073f6c1c2fa807ae6f71ccb5))
* **release:** skip tray persistence on github runners ([#30](https://github.com/bestlux/boundless/issues/30)) ([873ceb1](https://github.com/bestlux/boundless/commit/873ceb1670dc35853756e6f40b3577d8bbf46498))
* **release:** split tooling from source checkout ([#23](https://github.com/bestlux/boundless/issues/23)) ([7ecbbb7](https://github.com/bestlux/boundless/commit/7ecbbb705eedbd5d23db777916c2ec3ef7aad8aa))
* **release:** tolerate headless tray validation ([#29](https://github.com/bestlux/boundless/issues/29)) ([976f59b](https://github.com/bestlux/boundless/commit/976f59b837966a55ae74d3cae575fdaeeedf6b40))
* **release:** tolerate missing arp install path ([#28](https://github.com/bestlux/boundless/issues/28)) ([9958db2](https://github.com/bestlux/boundless/commit/9958db296558070946714aa769ebff58eee26743))
* **release:** update existing draft releases in place ([#33](https://github.com/bestlux/boundless/issues/33)) ([bd5b674](https://github.com/bestlux/boundless/commit/bd5b674d753056931e5f32234cbd638159d0fa00))
* restore ci and release workflows after merge ([#41](https://github.com/bestlux/boundless/issues/41)) ([0d6d7fa](https://github.com/bestlux/boundless/commit/0d6d7fa7118ad35d59008caabf24014a28444822))


### Miscellaneous Chores

* trigger 3.0.0 release ([131a2c8](https://github.com/bestlux/boundless/commit/131a2c83ab9242e778ff40cbcb490d61c2649c50))

## [2.1.0](https://github.com/bestlux/boundless/compare/v2.0.4...v2.1.0) (2026-03-19)


### Features

* **daemon:** reduce peer control latency ([14fa18b](https://github.com/bestlux/boundless/commit/14fa18bc7c262d69a60cd143c0af612bca657f91))
* **release:** ship windows msi releases ([#16](https://github.com/bestlux/boundless/issues/16)) ([a91d5b7](https://github.com/bestlux/boundless/commit/a91d5b79ca0b17c16919c08bf251e5a8c79fe433))


### Bug Fixes

* **ci:** stabilize post-refactor validation ([e3ef94f](https://github.com/bestlux/boundless/commit/e3ef94f56937303deef99a2e3b9b591b8708faf3))
* hide self and paired peers from discovery targets ([43a2669](https://github.com/bestlux/boundless/commit/43a26692819f59f8ab1261c444fbcf70da875ffd))
* **release:** drop broken cargo-workspace plugin ([#19](https://github.com/bestlux/boundless/issues/19)) ([a9f8637](https://github.com/bestlux/boundless/commit/a9f86374d3017f810add35fa83ff8ba1aaee34d7))
* **release:** restore literal crate versions ([fc6b821](https://github.com/bestlux/boundless/commit/fc6b8214c36890d3c6085ba6e1086ac98b33f48d))
* **release:** restore literal crate versions ([03df7f0](https://github.com/bestlux/boundless/commit/03df7f0ea59955c267b4dd1681784eccc70768ba))

## [2.0.4](https://github.com/bestlux/boundless/compare/v2.0.3...v2.0.4) (2026-03-08)


### Bug Fixes

* **ci:** split smoke coverage by stability ([#12](https://github.com/bestlux/boundless/issues/12)) ([f8b4a51](https://github.com/bestlux/boundless/commit/f8b4a51c4dca378000bf44700123c3f311579943))
* **ci:** stabilize dependency verify path ([#10](https://github.com/bestlux/boundless/issues/10)) ([a3d1482](https://github.com/bestlux/boundless/commit/a3d1482d5f8e2d5867cf0d5dd6f964c985d65050))
* restore stable release-please workflow ([138a5a8](https://github.com/bestlux/boundless/commit/138a5a8ef14e3e9ed1216c8e7e383e21f00d6dcb))
* set release package name explicitly ([79fc31c](https://github.com/bestlux/boundless/commit/79fc31c64eba0486550ab48e7905ed59329c16ce))
* use supported rust codeql build mode ([63b5276](https://github.com/bestlux/boundless/commit/63b5276d410c8c58b9dcb9e264f4876e3f489482))

## [2.0.3](https://github.com/bestlux/boundless/compare/v2.0.2...v2.0.3) (2026-03-08)


### Bug Fixes

* **tray:** refresh window and icon assets ([e9fa360](https://github.com/bestlux/boundless/commit/e9fa3603de9a9fe22ac35893c3a0eddc50a41f42))

## [2.0.2](https://github.com/bestlux/boundless/compare/v2.0.1...v2.0.2) (2026-03-08)


### Features

* **tray:** ship branded Windows icon assets ([31e195b](https://github.com/bestlux/boundless/commit/31e195bafd0356b6e79a1df5f498030f3541cc35))

## [2.0.1](https://github.com/bestlux/boundless/compare/v2.0.0...v2.0.1) (2026-03-07)


### Bug Fixes

* **packaging:** restore tray menu and add app shortcuts ([af17db3](https://github.com/bestlux/boundless/commit/af17db36a6fc8896d005cddee4175b2ed48f861b))
* **tray:** improve onboarding and window lifecycle ([81f02bb](https://github.com/bestlux/boundless/commit/81f02bb105bf951bafad00ded59a4a375c02ca3f))
* **tray:** use native window show/hide on windows ([39d054c](https://github.com/bestlux/boundless/commit/39d054cf603fb4c3e7c0e5b714ee0afe5bdb96d7))

## [2.0.0](https://github.com/bestlux/boundless/compare/v1.1.0...v2.0.0) (2026-03-03)


### ⚠ BREAKING CHANGES

* pre-reset protocol/config compatibility paths are removed; existing local configs may require clean bootstrap.

### Features

* **diagnostics:** surface pairing nonce/code failure breakdown ([4372ac1](https://github.com/bestlux/boundless/commit/4372ac1b2f6cf6dfd5ceb62681602dfb02865e06))
* **pairing:** bind nearby code confirmation to nonce ([eac0bb2](https://github.com/bestlux/boundless/commit/eac0bb234ae04e9cd19135f578613ad094d7aaec))
* **release:** add windows package bundle ([5de9800](https://github.com/bestlux/boundless/commit/5de9800a889f37c7ee190144f6630cfb0c8807a8))
* **scripts:** add input latency and jitter budget gates to trace profile ([e6b28d8](https://github.com/bestlux/boundless/commit/e6b28d87c8a80c38af46d811b89ccffe0bf14fa4))
* **scripts:** export input trace latency matrix as csv and json ([c78dd82](https://github.com/bestlux/boundless/commit/c78dd8272f2ef305206a9f6e649a26b1ef386f29))
* **security:** revoke trust records when removing peers ([9fb5641](https://github.com/bestlux/boundless/commit/9fb564170e945099b24e49d661fa2183bd64bc25))
* **test:** add success and lockout recovery modes ([7e82a69](https://github.com/bestlux/boundless/commit/7e82a69fc31b3b78d6101ed3b836ece4e0115463))
* **test:** automate pairing recovery matrix workflow ([e645992](https://github.com/bestlux/boundless/commit/e6459922df26131ac71ba86c26893637c3d0189e))
* **tray:** add actionable pairing recovery error messaging ([79f3419](https://github.com/bestlux/boundless/commit/79f3419e464a91e43bcaeb2b27f6b3386d9121b0))
* **tray:** add guided pairing retry loop for recoverable failures ([6b52907](https://github.com/bestlux/boundless/commit/6b529079cf1c20a68c335b9764ad0eeb5c041a55))
* **tray:** canonicalize layout local tokens and harden apply flow ([cd2916d](https://github.com/bestlux/boundless/commit/cd2916d99412fc5cd55e9e8496f4933d73aa5913))
* **tray:** introduce dashboard ui for onboarding and control ([ce3c963](https://github.com/bestlux/boundless/commit/ce3c963a359d742107984b033211552c6e993a5f))
* **tray:** pair discovered peers via direct API and wire flow ([284b4be](https://github.com/bestlux/boundless/commit/284b4be0daf753cd4a8f8ff6d6d01fe088acaeaa))


### Bug Fixes

* CI Clippy ([#4](https://github.com/bestlux/boundless/issues/4)) ([aff8e5c](https://github.com/bestlux/boundless/commit/aff8e5c92347b4e44965d7a7ae04a7f663d76902))
* **cli:** bound nearby pairing wire calls with timeouts ([49d6539](https://github.com/bestlux/boundless/commit/49d6539534da42a3da39f19e1a6ec280a2d48925))
* **daemon:** chunk oversized clipboard image transport ([9ae84b5](https://github.com/bestlux/boundless/commit/9ae84b5f60883191a062a326de71be3264973d4f))
* **daemon:** clear capture target during all-peer reconnect reset ([7e7768f](https://github.com/bestlux/boundless/commit/7e7768f9b8bd77ba919df03253d9f613c634bc43))
* **daemon:** close clipboard reconnect replay gaps ([58b72f5](https://github.com/bestlux/boundless/commit/58b72f5f45b245b856aacb31810d690102ea53c1))
* **daemon:** enforce clipboard conflict semantics ([f95ebf1](https://github.com/bestlux/boundless/commit/f95ebf1e96ae7ed3e6d6a76252ea0e19630ff9bb))
* **daemon:** replay latest clipboard snapshot on reconnect ([d9456c7](https://github.com/bestlux/boundless/commit/d9456c7007a228473dc3c1a7e5d47987a837ad41))
* **daemon:** tighten clipboard replay supersession ([b8df638](https://github.com/bestlux/boundless/commit/b8df6387dbe35e86908d4364526edafc362bcfb0))
* make tray test modules rustfmt-safe ([21f1707](https://github.com/bestlux/boundless/commit/21f17070aa3ac410d1a7f0a0d5f425624ea4f10d))
* **pairing:** expose nearby verification code to loopback tcp clients ([b86239e](https://github.com/bestlux/boundless/commit/b86239e21bc5f5a67888ba4aba45a020a9595263))
* retry reconnect image smoke assertion ([0e986fb](https://github.com/bestlux/boundless/commit/0e986fbb0659c6926f4cea98d941011f15d38319))
* **scripts:** make trace budgets clock-skew safe ([e9b80ef](https://github.com/bestlux/boundless/commit/e9b80ef8bc7d852ea7912849fb77189ed10b952f))
* stabilize verify checks ([e59a3f5](https://github.com/bestlux/boundless/commit/e59a3f5d14d46e796d3e3a971e10decbded2f771))
* **test:** make recovery automation resilient to hidden-code flows ([9bc27f1](https://github.com/bestlux/boundless/commit/9bc27f1266c2af22e8b9ea6bbeccb7515ebbf4c4))
* **test:** robustly parse capture output paths ([ec8a3aa](https://github.com/bestlux/boundless/commit/ec8a3aaaa98dc44666e3bcc2eb2b76dd1791ee03))
* **tray:** bound nearby pairing wire calls with timeouts ([d1509b5](https://github.com/bestlux/boundless/commit/d1509b509396357449b3cc5f158d180db25d7ef1))
* **tray:** harden pairing onboarding state flow ([323a43c](https://github.com/bestlux/boundless/commit/323a43cfbdb3d61756b9672532516105f14c1f1f))
* **tray:** make code-submit flow immediate and retry-safe ([1fcf963](https://github.com/bestlux/boundless/commit/1fcf9630930a3b28f140918c1242a9ea6744a02c))
* **tray:** use canonical nearby pairing wire op tag ([72e87fb](https://github.com/bestlux/boundless/commit/72e87fbc77fe026db782d5b196fb7b3f277b4c31))


### Code Refactoring

* enforce canonical v1 contract and remove legacy compatibility ([cb5dbe9](https://github.com/bestlux/boundless/commit/cb5dbe92aedc283d6c1a87485e50875f273dd638))

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
