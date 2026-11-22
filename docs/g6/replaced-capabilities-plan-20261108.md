# 被替换能力的认领清单（EchoMuse 退役后，2026-11-08）

> 用户点名（2026-11-08）：我们替换了 EchoMuse，它的一些技术功能要**设计
> 进来**——联网、其他配置。原则：**换组件先列它的能力清单逐项认领，
> 否则能力跟着退役静默丢失**（lessons G6-15）。

| # | 能力 | EchoMuse 时代怎么做 | 现状（G6 装机后） | 认领设计（去处与排期） |
|---|---|---|---|---|
| 1 | **WiFi 联网/换网** | Android WiFi 栈＋controller 串口 `em-wifi` | emOS 用 /data 既有 Android wpa 配置（首启无感回归已实证）；**换网目前只能串口 em-wifi** | 三层：①串口 em-wifi＝工程兜底（在位）；②**v0 协议 wifi 配置帧**＝在线换网主径——**已完成（C 阶段 2026-11-08）**：v0.2.3 `wifi_config/wifi_state`（写 /data/misc/wifi/wpa_supplicant.conf＋wpa_cli reconfigure＋20s 看门狗自动回滚）；实测假 SSID 全链回滚不失联、回环控制链不断（合同补篇见主仓 contracts-v0.md）；③AP 配置模式＝离线可配（产品级，C 阶段后按需）。注：②有鸡生蛋边界——设备不在线时只能靠①③，分层就是这个原因 |
| 2 | **设备时钟** | Android 时间服务 | nptd 对家用网关超时，时钟停 2010；不阻塞现状（固件↔core 无 TLS、云 API 在 Mac） | **固件北向取时**：core 连接建立后下发时间戳帧，固件 settimeofday（root 可写）——AEC 轮顺带实现；C 阶段 core 上机后本机 NTP/取时兜底。**C 阶段 TLS 依赖此项，硬前置** |
| 3 | 采集增益远程配置 | controller config（ADC_DIGITAL_GAIN/ADC_MICPGA，env＋消息） | 固件 bringup 固定 88/40（与 VAD 标定耦合） | **不做成运行时配置**（决策）：增益与声学标定绑定，变更走固件更新＋重标。记录在案，不建配置面 |
| 4 | **固件（音频组件）更新** | A/B 槽＋controller OTA | A/B 槽监督已在（快退×3 换槽）；core 侧写槽＋翻链＋触发——**已完成（C 阶段 2026-11-08）**：GET/POST /api/v0/firmware/audio|core（§12 原子链：同文件系统 tempfile→fsync→rename 盖非活跃槽→temp symlink＋rename 翻 current→TERM 触发受监督重启）；控制台诊断页维护区；真机双向验证（固件同版回写换槽＋核心自更新）；**近期开发回路＝物理门 TWRP→adb push→重启（~2min/轮，已实证）** |
| 5 | OS 镜像更新 | emOS 自带网络更新（controller 重建镜像→设备拉取→init trial 保底） | 未接入（镜像刚装机） | C 阶段接入：**未做（如实记录）**——core 已上机但 OS 镜像更新管线（controller 重建镜像→设备拉取→init trial）属 emOS 构建工具链工程，本轮排期让位于向导/固件更新/换网三项；遗留为 C 后首项。串口不作常规升级通道（一页纸 §3-2 不变） |
| 6 | 诊断 | controller 面板 | `pcmprobe`（二进制已在 /data）＋串口 emconsole＋控制台事件页/设备页 `(emos)` 形态标签 | 在位；AEC 轮补 `--aec-measure` 双轨记录（ERLE 测量入口） |
| 7 | 硬件静音 | 8×ADC mute（mute.go，C5 修复版） | G6 已做：Mute 键→ADC mute＋上行停＋灯语＋MuteState 北向＋持久化 | 完成（真人抽查待用户） |

## 排期归并

- **AEC 轮（G6 收尾）**：#2 取时帧（TLS 前置）＋#6 aec-measure。
- **C 阶段**：#1-② 在线换网帧✅、#4 core 侧更新 API＋控制台页✅、#5 OS 更新接入（未做，遗留）。
- **C 后按需**：#1-③ AP 配置模式。
- 不做：#3（决策记录）。
