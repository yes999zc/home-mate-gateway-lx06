# G5-2 · 替换 EchoMuse——直占音频组件设计（2026-11-08，用户拍板后实施；同日晨修订吸收式架构）

用户 2026-11-08 授权直接动手（G5-2＋AEC 数据＋开机自启一并做）；**同日晨追加方向决策**：①C（核心上音箱）升为商用关键路径——终态＝音箱上只有 hm-core＋自研音频组件两个进程、v0 协议本机回环、三服务全云（Mac/NAS 退场）；②**B 按吸收式架构收敛**——音频组件吸收 controller 角色、单进程直连北向，不留三层中间件；③EchoMuse 整体退役但保留在设备上作回滚通道；④监督/自启自带（A/B 槽＋快退回滚思想、不依赖 Magisk），core 上机复用同一套机制；⑤eMMC 治理第一天按商用标准。本文＝只读探测证据→设计定稿→实施与回滚配方。提案背景见 `on-device-form-20261107.md`（形态 B）。

## 1. 只读探测证据（2026-11-08 上午，live 设备，USB adb）

全部命令只读（cat/ls/strings/拉取二进制），未动设备状态。

| # | 事实 | 证据 | 来源 |
|---|---|---|---|
| E1 | 采集 PCM＝`pcmC0D24c`：RW_INTERLEAVED / **S24_3LE** / **9ch** / 16k / period 512 / buffer 2560，常开 RUNNING | `/proc/asound/card0/pcm24c/sub0/hw_params`+`status`（owner=EchoMuse server pid） | 本次 |
| E2 | 播放 PCM＝`pcmC0D23p`：RW_INTERLEAVED / S16_LE / **2ch** / 48k / period 2048 / buffer 8192 | 同上 pcm23p | 本次 |
| E3 | server 持久 fd＝controlC0＋pcm24c＋pcm23p＋`/dev/input/event1`+`event2`（按键）；**无 LED 节点** | `ls -l /proc/1191/fd` | G2 已有＋本次复核 |
| E4 | LED＝i2c `is31fl3236`（36 通道＝12 灯环×RGB），内核驱动 sysfs 暴露 `frame`/`led_current`/`boot_animation`；路径 `/sys/devices/soc/11007000.i2c/i2c-0/0-003f/` | sysfs ls＋driver 符号链接＋name 文件；server 二进制 strings 含该路径 | 本次 |
| E5 | **frame 格式＝72 个 ASCII hex 字符＋CRLF＝36 字节逐通道 PWM**（现值全 0＝灭）；led_current="3"、boot_animation="0" | `cat`+本地 od | 本次 |
| E6 | 开机自启槽位＝`/init.csm.project.rc:413` `service echomuse /data/local/bin/start_server.sh`（root/root system/late_start）——**init 定义在只读分区，脚本在 /data 可写** | grep init rc；`getprop init.svc.echomuse`=running | 本次 |
| E7 | 音量律：level 0–127＝0.5dB/步（0=−63.5dB、127=0dB）。**晨间修正**：volume_set 64→114 时全量 mixer diff＝零控件变化→EchoMuse 在 DSP 软件实现音量，`HP Driver Gain`(常值 6) 不参与；我方组件同样用软件增益（level_to_gain） | tinymix 两级快照 diff＋rui-dot2-bridge `volume.py@324d431`（只读引用） | 本次 |
| E7b | 晨间"LED 亮但无声"事故＝当时音量被设为零（用户澄清）；链路无病——wake/对话/出声均正常 | 用户 2026-11-08 晨间实测 | 用户 |
| E8 | mixer 全量 242 控件快照（含 EchoMuse 初始化序列全部控制名：Audio_I2S1_Setting/HP DAC/MFP Gpio/PCM Playback/四 ADC×Digital 88/MICPGA 40） | `tinymix` 全 dump 存档 `deploy/dot2d/direct/tinymix-full-20261108.txt` | 本次 |
| E9 | 原版 `start_server.sh` 全文到手（等待 echoaudioservice/wake_lock/混音初始化/kill ledcontroller/日志帽/A-B 槽监督/amp_off 保护/SIGTERM 处理） | 拉取存档 `deploy/dot2d/direct/start_server.sh.echomuse-orig` | 本次 |
| E10 | wake_lock 现持 "EchoMuse disable_suspend"；`tinymix` 在 `/system/bin/` 可用 | cat wake_lock；ls | G4＋本次 |
| E11 | codec＝TLV320AIC3101（strings "TLV320AIC3101 Capture"）；server 含 `dot2_hw_ref`/`VAD_THRESHOLD`/`AEC_ENABLED`/onnx 等旋钮（内部机制，不复用） | server strings | 本次 |
| E12 | 9ch 上行融合：原固件内部做（七麦融合 mic＋ch8 参考的语义来自旧 RUI 文档）；**直占后融合归我们**——通道语义待 A/B 窗口实测（能量＋与播放的相关性定位 ref 通道） | HANDOFF §6＋提案 §4 | 待实测 |

## 2. 设计定稿

### 2.1 形态：单进程吸收式（2026-11-08 晨修订，取代早前两进程设计）

```
[hm-core] ←— v0 LDJSON（现在拨 Mac relay 192.168.1.20:8932；C 阶段＝同协议本机回环，协议零改动）
    ↑
[gateway firmware]（唯一直占进程；吸收 controller 角色）
    ├ /dev/snd/pcmC0D24c 采集 9ch S24_3LE 16k → downmix → AudioChunk 北向
    ├ /dev/snd/pcmC0D23p 播放 2ch S16 48k ← Play WAV（voice/media 双通道混音）
    ├ /sys/…/0-003f/frame LED 36ch PWM ← AssistantState（调色板＋保留绿）
    ├ /dev/input/event1+2 按键 → Button/音量键
    ├ 软件音量增益（见 2.2/E7 修正）；持久化 state.json
    └ /sys/power/wake_lock
```

- **北向直连**：hello/描述符/重连真值恢复/Play 双通道回执/Stop/MicSession/AssistantState/ModeSet 全部从适配器移植（emuse/mod.rs 北向逻辑），协议 v0 一行不改。
- **描述符诚实**：capabilities＝`audio_in, audio_out, stop, volume`；firmware 标签＝`hm-firmware <ver> (direct)`——控制台可区分形态。
- **回滚通道＝EchoMuse 三件套原样保留在设备**（server_a/b＋原版脚本备份 start_server.echomuse.sh＋controller.json 环回指）；降级链＝G5-2 直占 →（监督回退）→ G5-1 共存 →（核心失联）→ 重拨循环。
- G5-1 适配器（adapter 子命令）保留用于 Mac 虚拟终端测试；设备上的 hm-adapter 进程在切换直占时停止（避免双网关呈现同一设备）。

### 2.2 音频管线

- **采集线程**（阻塞 readi，512 帧/块）：S24_3LE 9ch → 逐帧 downmix（配置的 mic 通道均值，24bit→i16 取高 16 位）→ 80ms 攒块（1280 帧）→ `[0x01][seq u16 BE][2560B]` /data。`--hw-ref` 时＝2ch `[mix|ch_ref]` 5120B（适配器 v0.2 已支持 ack `dot2_hw_ref`）。
- **播放线程**（阻塞 writei，2048 帧＝一个 period）：voice 0x02 与 media 0x04 两路入混音队列，sum(i32) 后 clamp、mono→stereo 复制写入；0x03/0x05 EOS 与 speaker_flush＝drop 队列＋drain；流结束回 `playback_stats{periods,underruns,bytesRecv,…}`（适配器靠它收 device_confirmed/PlayFinished）。underrun(-EPIPE)→prepare 恢复并计数。
- **音量（E7 修正后）**：**软件增益**（mixer 全量 diff 证据：level 114 vs 38 零控件变化→EchoMuse 在 DSP 内做音量）；律＝level 0–127 映射 0.5dB/步（127=0dB、0=−63.5dB），混音阶段乘法实现。VolumeSet→clamp+cap→持久化 state.json→**同步 device_confirmed 回执**（自进程内生效＝即时硬件事实，无待答窗口→适配器的 P3-3 位移语义不适用）。开机恢复持久值并在 hello 后上报。按键 VOL±＝level±8。
- **增益锚定**：启动脚本复刻原 tinymix 初始化（I2S1/HP DAC/MFP Gpio/PCM 100/ADC 88/MICPGA 40）——与 G4 声学标定（VAD −42/−47）同采集增益，可比；播放链新增软件音量层（E7 修正）。

### 2.3 LED／按键／电源

- LED：`leds{leds:[{id,r,g,b}],listening}` → 查通道映射表 → 36 字节 → 72 hex＋CRLF 写 sysfs。默认映射＝`[3k,3k+1,3k+2]=LED k (R,G,B)`；A/B 窗口用通道扫描＋摄像头视频校准后固化 `led-map.json`。
- 按键：event1+2 读 EV_KEY；113(KEY_MUTE)＝action（适配器映射 clickType 113→action_press）、115/116＝音量；未知码先只记录。
- 电源：启动脚本持 wake_lock（init 上下文）；每次启动 kill ledcontroller（防双写 LED sysfs）。

### 2.4 监督与开机自启（E6＋E9）

`/data/local/bin/start_server.sh` **内容替换**（原件备份为 `start_server.echomuse.sh`，init 服务定义不动）：

1. 等 `/dev/snd/controlC0`（替代等 echoaudioservice——同义：codec 就绪）
2. wake_lock "hm-direct"、tinymix 初始化序列、kill ledcontroller
3. **A/B 槽监督**（自带、不依赖 Magisk——init 服务槽位在只读 rc 但指向 /data 脚本）：`audio_a/audio_b`＋`current` 符号链接；≥15s 运行＝正常崩溃重启；一轮 3 次 <15s 快退→换槽；两轮全败→**exec 原版 EchoMuse 脚本**＝自动降级回 G5-1 共存形态
4. SIGTERM：杀子进程＋amp_off（PCM 0＋Ext_Speaker_Amp Off，复刻 E9 的保护）
5. eMMC 治理：chatty 日志只进 /tmp（RAM，5MB 帽＋512KB 转存）；/data 仅监督决策日志（64KB 帽、写前裁剪）＋state.json（音量，变更才写）——C 阶段 hm-core 上机时沿用同一套机制，SQLite WAL 等治理进 C 的验收清单

**降级链**：G5-2 直占 →（固件快退×3）→ G5-1 共存 →（Mac 核心失联）→ 设备重拨循环（G3 已验证行为）。任一层都可 USB adb 恢复。

### 2.5 回滚配方（人工，秒级）

- 回 G5-1：`stop echomuse`（su）→ `mv start_server.echomuse.sh start_server.sh` → `start echomuse`（原版脚本自带槽位监督拉起 EchoMuse）。
- 全原始（RUI 禁用态之前不在范围；RUI 禁用态＝controller.json 指回 .168，已有 G3 存档配方）。

### 2.6 AEC（本轮范围＝数据采集）

`--hw-ref` 2ch 采集落地后，A/B 窗口做：①ch 语义定位（播放正弦同时采 9ch：mic 通道声学延迟相关、ref 通道零延迟高相关）；②回声路径数据落盘样本（喂给后续核心侧 AEC 评估，处理实现按 g5 提案 §4 仍留独立决策）。**本轮不实现 AEC 滤波**。

## 5. ioctl ABI 破解实录（2026-11-08，alsa.rs 的证据基础）

`gateway pcmprobe` 子命令（保留为诊断工具）穷举＋系统二进制反汇编的完整结论：

| # | 事实 | 怎么知道的 |
|---|---|---|
| I1 | 厂商内核 `snd_pcm_hw_params`＝**604 字节（32 位任务）/608 字节（64 位任务）**，非主线 392 | 尺寸穷举：仅该两尺寸脱离 ENOTTY（EINVAL＝已进分发器） |
| I2 | 布局＝masks@0x04/0x08(access)、0x24/0x28(format)、0x44/0x48(subformat)；intervals@0x104 起 12×{min,max,flags}；rmask@0x200、cmask@0x204、info@0x208 | 反汇编系统 `libtinyalsa.so` 的 `param_init`（0x2f18）与 `pcm_open` 写入序列 |
| I3 | **hw_params 只接受 refine 过的缓冲**：info/masks 等字段需内核亲手合法化；正确流程＝param_init→HW_REFINE→钉 interval 值→HW_PARAMS | 直接按布局填值→EINVAL；refine 后钉值→rc=0 |
| I4 | **采集侧格式枚举移位**：S24_3LE＝bit **32**（word1 bit0）；播放侧格式＝主线编号（S16_LE=bit2） | `tinypcminfo -D 0 -d 24/23` 约束打印 |
| I5 | ioctl 编码＝asm-generic（dir<<30）——arm32 专属编码（DIRSHIFT=29）反被拒 | ctl pversion/elem_list 双编码对照 |
| I6 | **被占 PCM 的 open 阻塞**（不返回 EBUSY）→ 组件启动＝"武装等待"，旧主一停自动接管 | shell `exec 3<>pcmC0D24c` 挂起实验 |
| I7 | **原生 64 位路径对 D24c 不可用**：系统自带 64 位 tinycap 同样 EINVAL（参数在 608 布局下被拒）；EchoMuse（32 位）一直走 compat 路径成功 | `tinycap` 实测＋EchoMuse /proc 状态对照 → **固件定 armv7 musl** |
| I8 | readi/writei 的 xferi＝12B（32 位）/24B（64 位），请求码尺寸必须匹配 | libtinyalsa movz/movk 常量（0x80184151） |
| I9 | 播放 START 于空缓冲＝立即 EPIPE → 播放只 PREPARE、首写启动；采集才显式 START | 实测错误路径 |
| I10 | 上行刻度：24 位原值直接映射 i16＝过热（−19dB）；`>>4`（×16 over top-16）＝**G4 标定基线（噪底 ≈−50dB）**，VAD −42/−47 直接可用 | 逐通道电平扫描＋三档增益实测校准 |

## 3. 明确不做（本轮）

- 不动 init rc（系统分区只读）；不动 Magisk；不动其余三台 Dot。
- 不实现 on-device oww/VAD/AGC（核心侧已有，协议 config 保守关闭语义照旧）。
- 不刷机、不动 OTA 槽。
- NAS 实例跟进＝独立决策（C4），不随本轮。

## 4. 测试面

- 单元：S24_3LE 解码/downmix、mono→stereo、LED 帧编码、音量持久化、混音 sum/clamp。
- 合同（tests/firmware_contract.rs，Mac 可跑）：假控制器 WSS（复刻适配器握手）＋假硬件（trait 注入）→ register 形状/能力诚实、mic_start 后 0x01 帧大小与 seq、0x02→播放统计回执、flush→pong、volume_set→tinymix 调用形状＋volume_state、leds→sysfs 写形状、0x04 media 混音路径。
- 真机：A/B 窗口回归（mic 流/唤醒自回环/TTS 探针/LED 全色序/音量往返）＋回滚演练＋重启自启验证。
