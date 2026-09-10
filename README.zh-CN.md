<!-- SPDX-License-Identifier: MIT -->
<!-- SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com> -->

# kcsdi-rs

[English](README.md)

用 Rust 编写的 KC901 图形界面和命令行工具，通过以太网或 USB 串口控制仪器，查看 S11、S21 和频谱曲线，保存测量数据。

![同一次 S11 扫描回放的中文阻抗和史密斯图界面](https://github.com/user-attachments/assets/e6cdfcb7-c2c0-416f-8de0-cea8c79d6592)

| 测量 | 视图与导出 |
| --- | --- |
| S11 | 阻抗、Smith 图、相位、回波损耗和驻波比 |
| S21 | 相位、损耗和群延迟 |
| 频谱 | 实时曲线，支持线性和对数频率轴 |
| 数据 | CSV、XLSX 和 Touchstone `.s1p`。CLI 可将四组复数测量合成 `.s2p` |
| 分析 | 曲线显隐、保持、最大值与最小值包络、标记、缩放、平移和自动适配 |
| 界面 | 中文和英文，保存设备，支持浅色、深色和跟随系统主题 |

S11 和频谱实机测试使用 KC901V，固件版本为 V1.6.1。S21、串口和局域网发现已做软件和本地回放测试。其他 KC901 型号仍需实机测试。

## 构建与连接

安装稳定版 Rust。在 Ubuntu 上，先安装构建依赖：

```sh
sudo apt-get install build-essential pkg-config libwayland-dev \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
    libxkbcommon-dev libssl-dev
```

构建并启动 GUI：

```sh
cargo build --release --workspace --locked
cargo run --release -p kcsdi-gui
```

可执行文件位于 `target/release`。将该目录加入 PATH 后，即可使用下文的 CLI 命令。

在 GUI 首页添加设备，填写 TCP 地址和端口或串口路径，打开设备后点击底栏的连接。发现功能可扫描 KC901V 广播并添加设备。仪器同一时间只能接受一个控制连接。

在左栏添加最多十条轨迹，选择各自的视图和颜色。运行时依次采集可见轨迹，条件相同的轨迹共用一次测量。每条轨迹保留最近一次完整扫描和独立的 Y 刻度。在右栏添加标记后，可以在图中拖动。按 F11 切换全屏。

界面语言默认跟随系统，可以在设置中选择英文、简体中文或跟随系统。其他系统语言使用英文。

使用 CLI 时，将 `192.0.2.10` 换成仪器地址。频率单位为 Hz。

```sh
kcsdi info --host 192.0.2.10 --port 901
kcsdi serial-ports
kcsdi info --serial /dev/ttyUSB0
kcsdi discover
kcsdi limits --model kc901v
kcsdi sweep s11 --host 192.0.2.10 --port 901 \
    --start 5000 --stop 100000000 --points 201 --out antenna.s1p
kcsdi sweep s21 --host 192.0.2.10 --port 901 \
    --start 100000000 --stop 500000000 --points 201 \
    --format delay --rbw 10k --out delay.csv
kcsdi sweep spec --host 192.0.2.10 --port 901 \
    --start 100000000 --stop 500000000 --points 201 --rbw 10k --out spectrum.csv
```

KC901V 固件 V1.6.1 接受的扫描参数如下：

| 模式 | 频率范围 | 最小跨度 | 点数 |
| --- | --- | --- | --- |
| S11 | 5 kHz 至 7 GHz | 1 kHz | 3 至 1001 |
| 频谱 | 0 Hz 至 7 GHz | 1 kHz | 3 至 1001 |

这些是实测的命令边界，测量精度请参照仪器规格。LOG X 改变显示刻度，采样间隔保持不变。

## 导出

选择 CSV 或 XLSX，可导出所选轨迹或全部可见轨迹。文件保留完整测量及其采集参数，不含保持、最大值和最小值包络。GUI 的 CSV 每行记录一个原始值，附带频率、单位和轨迹信息。XLSX 为每条轨迹建立数据表，并用 Metadata 表记录采集参数。

完成 S11 相位、Smith 图或阻抗扫描后，选择 Touchstone 可导出 `.s1p`，保留所选完整轨迹的全部采样点和复数分量。单独的回波损耗或驻波比数据缺少相位。

S21 的 CLI 导出使用 CSV，相位单位为度，群延迟单位为秒，损耗保留仪器返回的正负号。

CLI 可以将频率点一致的四组复数 CSV 测量合成双端口文件：

```sh
kcsdi export s2p --s11 s11.csv --s21 s21.csv \
    --s12 s12.csv --s22 s22.csv --out network.s2p
```

完整双端口采集尚未实现。CLI 的 Touchstone 导出默认为 2.0 版，参考阻抗为 50 欧姆。输入格式和独立验证方法见 [Touchstone 导出](docs/touchstone.md)。

## 参考与许可

协议实现参考科新社的 [KC901 编程手册](https://www.measall.com/)（B002-008，第三版），并结合 MEASALL Technology 的 [KCSDI](https://deepace.net/products/software/kcsdi-software/) 所记录的设备行为进行交叉验证。界面布局也参考了 KCSDI。感谢这两个项目对仪器行为的记录。

Rust 代码采用 [MIT 许可](LICENSE)。字体和来源说明见 [NOTICE](NOTICE)。
