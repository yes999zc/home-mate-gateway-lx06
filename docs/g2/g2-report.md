# G2 完成报告（2026-11-07）

G2 范围（网关 HANDOFF §3）：旧协议只读取证、dot2-d 现连接只读观察、接入方案设计与影响评估。四项交付全部完成，本报告汇总＋给出下一步决策面。

## 1. 交付物

| 文档 | 内容 |
|---|---|
| [dot2d-observation-20261107](dot2d-observation-20261107.md) | 实机观察快照：身份/固件栈/监督机制/资源/配置/连接现状；程序 SHA256 首次重算 |
| [echomuse-protocol-forensics](echomuse-protocol-forensics.md) | 线协议取证：传输/鉴权/帧格式/消息全集/音量响度链/LED，来源提交号齐全 |
| [protocol-mapping-v0](protocol-mapping-v0.md) | 映射草案：23 项帧级映射、能力映射、声学职责、Path A/B、缺口 G1–G8、验收要点 |
| 本报告 | 结论＋确认清单＋下一步 |

## 2. 一句话结论

dot2-d 是一台 root 可用、A/B 槽监督、纯外拨 WSS 的 EchoMuse 客户端；**接入 Home Mate 不需要动固件**——网关实现控制器角色（WSS `/control`＋`/data`＋token＋TLS）并把设备 `controller.json` 重指过来即可；唯一硬前置是网关协议 v0 缺上行音频通道（缺口 G1，触发 v0.2）。

## 3. 关键事实（证据见对应文档）

1. **设备现状**：FireOS 5.1.1（mt8163），Magisk root；EchoMuse `server_b`（SHA256 `573277…37486`，首次重算，≠基线继承值）；RSS ~27MB（Go）；已从 RUI 禁用，正对 `192.168.1.10:9014` 每分钟空转重试（refused）。
2. **线协议**（DOT@324d431 取证）：双平面 WSS；`/control` JSON（register/ack/config/mic_start/ping/speaker_flush/volume_set/leds…）；`/data` 二进制（`[0x01][seq BE][PCM 16k]` 80ms mono/2ch、`[0x02][PCM 48k ≤4096B]`＋`[0x03]` EOS、哨兵 0x04/0x05）；seq 头不是音频；设备 prime ~1.1s、EOS 后重 prime 30–800ms。
3. **声学职责现役布局**：设备 AEC 关、七麦融合＋ch8 hw 参考上行（`dot2_hw_ref`）、控制器侧 AEC；音量设备自有 0–127（0.5dB/步）＋控制器上限/响度 DSP。
4. **授权状态（本会话用户指令）**：dot2-d 自由使用（含刷机/移除 EchoMuse）；RUI 侧已禁用且留有恢复路径；后续探索服务上音箱本体（Path B，后置）。

## 4. 实机接入前确认清单（按新授权更新）

**已获用户预授权（无需再问）**：dot2-d 本机一切动作——改 `controller.json`/`ca.pem`/`token`（带备份）、push/写文件、重启 server 进程、乃至刷机/移除 EchoMuse（影响大者仍会先在汇报中说明再动手，属沟通礼节而非权限门槛）。

**仍需用户点头的动作**（跨界即问）：

| # | 动作 | 为什么 |
|---|---|---|
| C1 | 从 RUI 生产机（.168）读取/复用其 TLS CA、证书或任何 RUI 侧文件 | 涉旧系统；Path A 已选自签 CA 方案避开此项 |
| C2 | 在 RUI 侧重新启用 dot2-d（`enabled:true`）或任何 RUI 配置变更 | 旧系统操作，与「不动旧 RUI」红线冲突 |
| C3 | 动其余三台 Dot（dot2/dot2-b/dot2-c） | 首期范围外 |
| C4 | NAS（.34）部署变更：网关进程上 NAS、新增监听端口 | NAS 隔离边界（nas-deployment §0）；Mac 本机监听端口不在此列 |
| C5 | 对家庭网络暴露新监听端口（非本机回环/内网特定主机） | 家庭网络影响 |

## 5. 下一步（G3 建议范围：协议 v0.2＋dot2 适配器＋首次实机建链）

1. **v0.2 上行音频**（缺口 G1，主仓+网关两侧）：`audio_chunk`＋`mic_session` 帧定稿（contracts-v0.md 治理）；核心 DeviceHub/语音管线接入连续 PCM；唤醒决策（G3 决策点：核心侧 KWS 首版，设备 oww 关）。
2. **网关 dot2 适配器**：WSS 服务端（自签 CA 流程脚本化）、token 读取/写回、帧编解码（seq 剥头）、16k→48k 重采样、音量映射、flush 三态回执；合同测试（映射文档 §6.1 全项）。
3. **首次实机建链**（观察快照 §4.1 流程）：重指 controller.json→register 实测→mic 流到核心→TTS 实播→flush 演练→**回滚演练一次**。
4. 验收如实分层：模拟（合同测试）/实机建链（register/mic/回执）/声学效果（延迟、唤醒率、打断）——第三层允许遗留到 G4。

Path B（上机研究）在 G3 验收后单独立项（G5），证据已备。
