# G6 · 物理兜底门实测记录（dot2-d 本机，2026-11-08）

设备 G090L91072732EXG。一页纸 §5 要求：装 emOS 前必须本机实测此门，
不许拿客厅机类推。监测＝`/tmp/hm-g6/gate-monitor.sh`（adb/fastboot 状态
逐秒记录＋TWRP 识别），全程日志存档于本会话 `/tmp/hm-g6/gate-monitor.log`。

## 结果：通过（第 2 次尝试）

| 尝试 | 时间 (UTC) | 操作 | 结果 |
|---|---|---|---|
| 1 | 01:59:44–02:00:11 | 断电→按住 Mute 上电→**5 秒松手** | **失败**——正常引导回 FireOS（`twrp=none build=LVY48F`，27s 后 adb 出现即识别） |
| 2 | 02:07:25–02:07:59 | 断电→按住 Mute 上电→**持续按住约 30 秒不松** | **成功**——`recovery` 态，`twrp=3.7.0_9-bboe2 build=omni_biscuit-eng`；断电到 TWRP adb 就绪 34s |

## 关键差异（与客厅机不可类推的直接证据）

- 客厅机（EMOS-CANDIDATE-20261104）：Mute 按住上电**约 5 秒松手**即见 TWRP。
- **本机：5 秒松手无效（直接回 FireOS）；必须持续按住不放（≥~30s，穿过
  整个早期引导）才进 TWRP。** 推测 LK 对按键的采样窗口/时机与客厅机不同，
  未深究根因（结论按实测行为记录）。

## 通过时的验证（TWRP 内，只读）

- p10 整读回 Mac：`fa5e512cbdf90a451fd16ca7edfeb6f52bf603e7976d3ece941c6e8f0dff64cf`
  ——与 11-04 NAS 权威基线字节级一致（进 TWRP 过程未动分区）。
- 随后 `adb reboot` 回 FireOS，hm-direct 自起、核心重连（既有自愈路径复验）。

## 生效的兜底门配方（本机）

> 拔电 → 按住 Mute 键（顶部带斜杠麦克风图标） → 插电上电 →
> **持续按住不放 ~30 秒**（看到灯环行为变化/不亮也继续按满）→ 松手 →
> TWRP（adb 可用，`/data` 可挂载，可 `dd` 恢复 p10）。

**可见指标（用户 2026-11-08 补充，实机确认）**：按住期间**灯圈变白**＝
已进入此门的确认信号——比等 adb 出现更早、肉眼即判。做门时盯白灯。

变体实测（补录）：
- **动作键上电：通过（2026-11-08 02:25）**——按住动作键上电 ~10 秒 →
  **灯圈变绿**（用户实机报告的可见指标）→ **fastboot**
  （`fastboot devices` 见 `G090L91072732EXG fastboot`，02:25:14）。
  只读探测：`product BISCUIT`、`secure yes`。`fastboot reboot` OKAY
  （02:26:50）→ emOS 自愈回归（核心侧 `(emos)` 重连）。**未实测
  flash 命令**（属写入动作，需要时再验）——门的存在以设备可达 fastboot
  为准。此门比 TWRP 更底层（可 `fastboot flash`），两道门互补：
  - Mute 按住 ~30s，白灯 → TWRP（adb、可挂 /data、可 dd）；
  - 动作键 ~10s，绿灯 → fastboot（可 flash）。
- 双键同按：未测。

## 结论

**物理兜底门＝通过，且有两道**（TWRP＋fastboot，白灯/绿灯各自确认）。
装机前置条件全部满足（备份 SHA 匹配＋还原演练 2 分钟路径＋物理门实测＋
镜像离线核验）。emOS 安装解除阻塞（已于 2026-11-08 02:14 完成，见
[安装记录](emos-install-record-20261108.md)）。

## 2026-11-14：真实故障下再次通过（M8b 镜像回滚）

不是演练，是一次真实的不开机：写入"DTB 关 uart0"的 p10 后设备未进系统（HTTP 无响应、
USB 串口不回来；另一个 `usbmodem8401` 属于另一台 Dot `G090LF0964340RN6`，别误判为"设备还活着"）。
用户按本文件 §"生效的兜底门配方"操作——**拔电 → 按住 Mute → 上电 → 持续按住约 30 秒**——
`adb devices` 出现 `G090L91072732EXG  recovery`（TWRP）。

恢复步骤（全部实测）：

```
adb push emos-v0.10-hm.1-nocache-p16m.img /tmp/orig-p10.img
adb shell 'dd if=/tmp/orig-p10.img of=/dev/block/mmcblk0p10 bs=4096; sync; md5sum /dev/block/mmcblk0p10'
# 读回 8b9004f8d24ec5127bc5f6675f6672f7 = 原始镜像
adb reboot
```

重启后 HTTP 200、`/dev/cu.usbmodem31101` 回来、`/proc/consoles` 仍是 `ttyMT0 klog-1 ram-1`，
活动发布 24 与健康检查全部恢复。**从进 TWRP 到写回校验约 2 分钟**，与 2026-11-08 的演练一致。

顺带确认的分区名（TWRP 视角）：`boot_a`/`boot_a_x` → p10，`boot_b` → p11；
`lk_a`/`lk_b` 是 **/tmp/ota-decoy 下的诱饵**，真正的 LK 是 `lk_a_real` → **p3**、`lk_b_real` → **p5**
（两者当前逐字节相同）。改 LK 时不能写 `lk_a`/`lk_b` 这两个名字。
