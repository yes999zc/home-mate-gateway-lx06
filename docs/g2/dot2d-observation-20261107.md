# G2 · dot2-d 实机观察快照（2026-11-07）

观察窗口：2026-11-07 21:00–21:15 北京时间（设备钟 13:00–13:13 GMT，二者一致）。观察者：Home Mate 接手 Agent（G2 会话）。

**方法与边界**：全部经 `adb -s G090L91072732EXG shell` 只读命令（getprop / cat /proc / ls / ps）；root-only 文件经 Magisk `su -c` 读取（仅读操作）。尝试过一次 `adb root`，被生产构建拒绝（"adbd cannot run as root in production builds"），无副作用。未 push / install / setprop / reboot / 改配置。首次 `adb devices` 枚举延迟约数秒后设备正常出现（state `device`，非 unauthorized/offline）。

**授权变更（本会话用户指令，晚于观察开始）**：用户已于 2026-11-07 将 dot2-d 从 RUI 禁用，并授权本设备自由使用（含刷机、移除 EchoMuse）；后续将探索把服务放上音箱本体。本快照中段之后的读取即在该授权下进行。仍守的边界：旧 RUI 机器/仓只读、其余三台 Dot 不动、NAS 部署隔离、凭证不进 Git。

## 1. 设备身份

| 项 | 值 | 来源 |
|---|---|---|
| ADB serial | `G090L91072732EXG`（usb:3-1.1，transport_id 9） | `adb devices -l` |
| product / model / device | `csm_biscuit` / `AEOBC` / `biscuit` | 同上＋getprop |
| USB | Amazon `AEOBC`，idVendor 6473 / idProduct 272，USB2 高速；独占 owner＝本机 adb（pid 5983） | ioreg |
| OS | FireOS（Android 5.1 时代）：`Amazon/csm_biscuit/biscuit:5.1.1/LVY48F/272.6.8.0_user_680767620:user/release-keys`，构建时间 1668746680（2022-11-18） | getprop ro.build.* |
| SoC | mt8163（ MediaTek，四核 ARM 32 位） | ro.hardware |
| 网络 | wlan0 已配网（wpa_supplicant 运行；dhcpcd hostname `amazon-8aedf16b2`）；`persist.adb.tcp.port` 为空＝网络 ADB 关闭 | ps / getprop |
| 启动 | sys.boot_completed=1；本次开机 2026-11-06 07:44（supervisor.log），uptime ≈1d5h | supervisor.log / uptime |

## 2. EchoMuse 固件栈（设备现状）

### 2.1 部署形态（/data/local/bin/）

```
server -> server_b        （symlink，2026-11-05 06:56）
server_a   12,558,619 B   2026-11-05 00:26
server_b   12,564,099 B   2026-11-05 06:56
start_server.sh 11,352 B  2026-10-29 13:57
```

- **程序 SHA256（本次实测重算，基线首次复核）**：
  - `server_b`＝`573277181fafc68185ed10a6e00b67636059bdb316100ea5c383c61b1dd37486`（活动槽）
  - `server_a`＝`b46b1ce23a18420031d9f596dd9faaf2ff15e15ee19368ba5aa4e886546a6ff3`
  - 与基线继承值 `297d53cf…` 均不同——预期内：继承值属旧程序，且基线已注明「未执行复核」。基线表应据本记录更新。
- server 为 **Go 二进制**（ELF 32-bit ARM；运行日志含 goroutines/heap/num_gc）。
- 当前实例 PID 5632，2026-11-06 08:40:48 起（slot=server_b）。

### 2.2 监督机制（start_server.sh 全文已读）

- **A/B 槽＋自动回滚**：server 运行 <15s 视为失败启动，连Fail 3 次换槽重启；≥15s 的退出只重启不换槽。
- **启动序**：等 echoaudioservice（FireOS 上 ≤120s）→ 硬件初始化：`ip link set p2p0 down`、wake_lock "EchoMuse"、tinymix（`Audio_I2S1_Setting On`、`HP DAC Playback Switch 1 1`、`MFP Gpio Mute On`、`PCM Playback Volume 100 100`；四路 ADC A–D `Digital Volume Control 88 88`、`MICPGA Volume Ctrl 40 40`）→ kill ledcontroller → 日志裁剪循环（/tmp/server.log 5MB 上限，512KB 转 .1）→ 起 server。
- **功放保护**：server 不在运行时 amp 静音（PCM Volume 0＋Ext_Speaker_Amp_Switch Off），防空闲嘶声。
- **emOS/FireOS 判别**：`/dev/__properties__` 存在与否（非构建标志双产物）。
- **持久监督日志**：`/data/local/etc/echomuse/supervisor.log`（64KB 上限）；/tmp 是 RAM 盘、重启即失。
- supervisor.log 全文已读：2026-10-29 起 20 次启动记录；11-04/05 有 A/B 槽往复（与 `20261105-volume-r1` 部署窗口吻合）；11-06 晨三次 code=0 干净退出后进入当前长运行。**无 fast-exit/rollback 记录**。

### 2.3 运行时资源

- server fd：`/dev/snd/controlC0`、**`/dev/snd/pcmC0D24c`（采集）**、**`/dev/snd/pcmC0D23p`（播放）**、`/dev/input/event1+2`（按键）、`/dev/__properties__`。＝直接占用 ALSA 设备与输入设备。
- 内存（server 自报，13:11）：goroutines=14，heap_alloc ~2.8MB，**RSS ~26.8MB**，num_gc≈26.9k，GC 累计暂停 9.2s。
- mic 时钟监控（每 ~2min）：`audio 102713.0s over wall 102691.1s (skew -21869ms capture fast, stalls=0, sub_drops=0)` → 采集钟快 ~**-214ppm**，无停顿/丢帧。
- Magisk root 在位（shell `su -c` 即得 uid=0, context u:r:magisk:s0）；adbd 本身生产模式。

### 2.4 配置与状态（/data/local/etc/echomuse/）

| 文件 | 内容/事实 |
|---|---|
| `controller.json`（世界可读） | `{"endpoints":[{"host":"192.168.1.10","port":9014,"tls_port":9014}],"mdns":false}` —— 设备**唯一静态端点＝RUI 生产机 .168:9014**，mDNS 关 |
| `state.json`（600, root） | `{"muted":false,"volume":114}`（设备自有音量，0-127 级） |
| `token`（600, root） | 64 字节，**未读取内容**（设备↔控制器鉴权凭证） |
| `ca.pem` | 554B（＋.bak）＝设备侧信任的控制器 TLS CA |
| `controller.json.before-*`×2 | .168:9014 之前的两次指向变更备份（household/nas 迁移痕迹） |

## 3. 连接现状（RUI 已禁用，设备空转重试）

- **socket 表**：`/proc/net/tcp` 空、`tcp6` 仅两条 uid=1000 到 AWS :80 的系统连接、UDP 表空 —— **设备当前无任何到 .168:9014 的连接**。ARP 表残留 .168 表项（缓存）。
- **server.log 尾部**（13:04–13:12 GMT，每分钟一条循环）：

  ```
  [control] Static endpoint 1/1 (attempt 1/2): 192.168.1.10:9014
  [control] Connecting to wss://192.168.1.10:9014
  [control] Connection lost: dial tcp 192.168.1.10:9014: connect: connection refused — reconnecting in 1m0s
  ```

  ＝**设备是纯外拨 WSS 客户端**（`wss://`，非明文 ws），每端点 2 次尝试、退避 60s，无监听端口。
- `[data] StartMic: no connection yet — will start on connect` —— mic 流随控制器连接建立才启动。
- 13:06:59 物理按键 clickType=113：`action button ignored — no controller session`；随后 `Mute: mic muted`→`unmuted`（按钮本地生效，事件无上报对象）。
- **RUI 侧对账**（rui-local@`db2038d` 只读）：用户经 `PATCH /api/v2/devices/dot2-d {"enabled":false}` 禁用（配置备份 `config.yaml.bak.disable-dot2d-20261107T135954Z` 在案）；`/health` devices=3 enabled=3；重新启用＝同一 API `enabled:true`。M6.1 中枢试用亦已单独关闭（`RUI_AGENT_BRAIN_DEVICES` 置空）。两侧证据互相印证：设备侧重试 refused、RUI 侧 enabled=false。

## 4. 对网关/上机设计的直接结论

1. **接入无需碰设备固件**：`controller.json` 是纯配置文件；把端点指向 Home Mate 网关＋网关实现 EchoMuse 控制器协议（WSS 服务端）即可建链。恢复＝改回配置（备份文件模式现成）。
2. **网关必须是 TLS WSS 服务端**，双平面 `/control`（JSON）＋`/data`（二进制），鉴权 `X-EM-Token`（token 在设备上可经 adb root 读取，无需碰 RUI 机器）。
3. **上机（Path B）可行性证据**：armv7 32 位＋Android 5.1＋Magisk root＋`/data/local/bin` A/B 槽部署模型＋监督脚本可整套复用；ALSA 采集/播放设备、按键 input 设备可直接占用；Go 版 EchoMuse RSS ~27MB 是我们 Rust 侧的对照预算（应显著低于此）。交叉编译需 armv7-androideabi 目标（M4 已有 cargo-zigbuild 经验）。
4. **时序基线**：设备重连周期 60s（改配置后需等≤1min 或重启 server 进程——重启属本机动作已授权）；mic 钟差 -214ppm（网关侧时钟跟踪参考）。

## 5. 未决/待核实

- dot2-d 是否宣告 `audio_mix` 能力（音乐 0x04/0x05 通道）——接入握手时从 `register.capabilities` 实测。
- 设备 oww（on-device 唤醒词）模型/阈值现值——在 `config` 推送语义里，须接入后回读 `stats`/行为确认；HM 唤醒词「高达」需自定义模型或核心侧 KWS（见映射文档决策点）。
- `token`/`ca.pem` 的取得与替换策略（映射文档 §TLS 与鉴权）：token 内容可从设备读；ca.pem 换成网关自签 CA 需写设备文件（已授权），或复用 RUI 侧 CA（涉旧系统，需用户点头）。
