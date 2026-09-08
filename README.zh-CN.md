<!-- SPDX-License-Identifier: MIT -->
<!-- SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com> -->

# kcsdi-rs

[English](README.md)

用 Rust 编写的 KC901 图形界面和命令行工具，通过以太网控制仪器，查看 S11 和频谱曲线，保存测量数据。

![KC901V 同一组 S11 测量的阻抗曲线和 Smith 图](https://github.com/user-attachments/assets/84feeb3d-4d14-41b7-902d-d50299f59330)

| 测量 | 视图与导出 |
| --- | --- |
| S11 | 阻抗、Smith 图、相位、回波损耗和驻波比 |
| 频谱 | 实时曲线，支持线性和对数频率轴 |
| 数据 | CSV 和 Touchstone `.s1p`。CLI 可将四组复数测量合成 `.s2p` |
| 分析 | 曲线显隐、保持、最大值与最小值包络、标记、缩放、平移和自动适配 |
| 界面 | 中文和英文，保存设备，支持浅色、深色和跟随系统主题 |

实机测试使用 KC901V，固件版本为 V1.6.1。其他 KC901 型号仍需实机测试。

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

在 GUI 首页添加设备，填写地址和 TCP 端口，打开设备后点击底栏的连接。仪器同一时间只能接受一个控制连接。

在左栏选择 S11 或频谱，通过轨迹菜单切换 S11 视图。两种模式各自保留最近一次扫描，只有当前模式采集数据。在右栏添加标记后，可以在图中拖动。按 F11 切换全屏。

使用 CLI 时，将 `192.0.2.10` 换成仪器地址。频率单位为 Hz。

```sh
kcsdi info --host 192.0.2.10 --port 901
kcsdi limits --model kc901v
kcsdi sweep s11 --host 192.0.2.10 --port 901 \
    --start 5000 --stop 100000000 --points 201 --out antenna.s1p
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

在相位、Smith 图或阻抗视图完成扫描后，点击导出 .s1p。导出文件保留完整扫描数据，不受曲线显隐和当前视野影响。单独的回波损耗或驻波比数据缺少相位。

CLI 可以将频率点一致的四组复数 CSV 测量合成双端口文件：

```sh
kcsdi export s2p --s11 s11.csv --s21 s21.csv \
    --s12 s12.csv --s22 s22.csv --out network.s2p
```

完整双端口采集尚未实现。导出默认为 Touchstone 2.0，参考阻抗为 50 欧姆。输入格式和独立验证方法见 [Touchstone 导出](docs/touchstone.md)。

## 参考与许可

协议实现参考科新社的 [KC901 编程手册](https://www.measall.com/)（B002-008，第三版），并结合 MEASALL Technology 的 [KCSDI](https://deepace.net/products/software/kcsdi-software/) 所记录的设备行为进行交叉验证。界面布局也参考了 KCSDI。感谢这两个项目对仪器行为的记录。

Rust 代码采用 [MIT 许可](LICENSE)。字体和来源说明见 [NOTICE](NOTICE)。
