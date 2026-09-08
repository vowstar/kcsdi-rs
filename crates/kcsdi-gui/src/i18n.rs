// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Small, typed UI catalog. Device commands, units, and diagnostic details
//! remain unchanged when the display language changes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
    #[default]
    #[serde(rename = "en", other)]
    English,
}

impl Language {
    pub const ALL: [Self; 2] = [Self::English, Self::SimplifiedChinese];

    /// Native language names keep the selector usable in either language.
    pub fn label(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
        }
    }
}

// Each row requires both translations, so adding a key cannot silently
// leave a language incomplete. Add a language by extending this catalog.
macro_rules! catalog {
    ($( $key:ident => ($en:literal, $zh:literal), )+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Text { $( $key, )+ }

        #[cfg(test)]
        impl Text {
            pub const ALL: &'static [Self] = &[$( Self::$key, )+];
        }

        impl Language {
            pub fn text(self, key: Text) -> &'static str {
                match (self, key) {
                    $(
                        (Self::English, Text::$key) => $en,
                        (Self::SimplifiedChinese, Text::$key) => $zh,
                    )+
                }
            }
        }
    };
}

catalog! {
    Devices => ("Devices", "设备"),
    Settings => ("Settings", "设置"),
    About => ("About", "关于"),
    Instrument => ("Instrument", "测量"),
    Function => ("Function", "功能"),
    TraceList => ("TRACE LIST", "轨迹列表"),
    TraceSettings => ("Trace settings", "轨迹设置"),
    FrequencyRangeTab => ("FREQUENCY RANGE", "频率范围"),
    SelectedTraceOnly => ("Run acquires the selected trace", "运行时采集所选轨迹"),
    Connection => ("Connection", "连接设置"),
    Show => ("Show", "显示"),
    Hide => ("Hide", "隐藏"),
    LocalDevices => ("Local devices", "本地设备"),
    AddDevice => ("Add device", "添加设备"),
    EditDevice => ("Edit device", "编辑设备"),
    DeviceName => ("Name", "名称"),
    NoSavedDevices => ("Add a TCP connection to get started.", "添加 TCP 连接以开始使用。"),
    OpenDevice => ("Open", "打开"),
    Edit => ("Edit", "编辑"),
    Delete => ("Delete", "删除"),
    DeleteDevice => ("Delete this saved device?", "删除此设备配置？"),
    DeleteDeviceHelp => ("The connection profile will be removed from this computer.", "将删除本机保存的连接配置。"),
    Save => ("Save", "保存"),
    Cancel => ("Cancel", "取消"),
    Theme => ("Theme", "主题"),
    ThemeSystem => ("System", "跟随系统"),
    ThemeLight => ("Light", "浅色"),
    ThemeDark => ("Dark", "深色"),
    Appearance => ("Appearance", "外观"),
    DeviceDetails => ("Device details", "设备信息"),
    AppDescription => ("Desktop control for KC901 instruments", "KC901 仪器桌面控制软件"),
    AppVersion => ("Version", "版本"),
    SoftwareVersion => ("Software", "固件"),
    HardwareVersion => ("Hardware", "硬件"),
    SerialNumber => ("Serial number", "序列号"),
    DeviceUser => ("Device user", "设备用户"),
    Temperature => ("Temperature", "温度"),
    DeviceInfoUnavailable => ("Connect an instrument to view its details.", "连接仪器后查看设备信息。"),
    Refresh => ("Refresh", "刷新"),
    ProfileNameRequired => ("Enter a name.", "请输入名称。"),
    ProfileHostRequired => ("Enter a host name or IP address.", "请输入主机名或 IP 地址。"),
    ProfileNameExists => ("A saved device already uses this name.", "已有同名设备。"),
    SingleSession => ("Disconnect the current instrument before opening another device.", "请先断开当前仪器，再打开其他设备。"),
    Step => ("STEP", "步进"),
    Scale => ("SCALE", "刻度"),
    Reference => ("REF", "参考值"),
    Divisions => ("GRIDS", "格数"),
    PerDivision => ("/DIV", "每格"),
    StepHelp => ("STEP is rounded to the nearest supported point count while preserving START and STOP.", "步进按最接近的有效点数取整，保持起止频率不变。"),
    InvalidStep => ("Choose a positive step that produces a valid point count.", "请输入对应有效点数的正步进值。"),
    AnalysisHold => ("HOLD", "保持"),
    AnalysisMaxHold => ("MAX", "最大保持"),
    AnalysisMinHold => ("MIN", "最小保持"),
    AnalysisReset => ("Reset holds", "重置保持曲线"),
    AnalysisMarkers => ("MARKER", "标记"),
    AnalysisAdd => ("ADD", "添加"),
    AnalysisRemove => ("DEL", "删除"),
    AnalysisClear => ("DEL ALL", "全部删除"),
    AnalysisFrequency => ("Frequency", "频率"),
    AnalysisValue => ("Value", "读数"),
    AnalysisDelta => ("Delta", "差值"),
    AnalysisMaximum => ("Maximum", "最大值"),
    AnalysisMinimum => ("Minimum", "最小值"),
    AnalysisLeftPeak => ("Left peak", "左侧最大值"),
    AnalysisRightPeak => ("Right peak", "右侧最大值"),
    AnalysisColumn => ("Readout", "读数分量"),
    AnalysisMagnitude => ("Magnitude", "幅值"),
    Language => ("Language", "语言"),
    Host => ("Host", "主机"),
    Port => ("Port", "端口"),
    Connect => ("Connect", "连接"),
    Disconnect => ("Disconnect", "断开"),
    Disconnected => ("Disconnected", "未连接"),
    Connecting => ("Connecting", "连接中"),
    Connected => ("Connected", "已连接"),
    Error => ("Error", "错误"),
    Firmware => ("sw", "固件"),
    ExternalPower => ("ext", "外部"),
    Battery => ("bat", "电池"),
    Spectrum => ("SPEC", "频谱"),
    Phase => ("Phase", "相位"),
    ReturnLoss => ("Return Loss", "回波损耗"),
    Vswr => ("VSWR", "驻波比"),
    Smith => ("Smith", "史密斯图"),
    Match => ("MATCH", "匹配"),
    Short => ("SHORT", "短路"),
    Open => ("OPEN", "开路"),
    Inductive => ("Inductive (+X)", "感性 (+X)"),
    Capacitive => ("Capacitive (-X)", "容性 (-X)"),
    Impedance => ("Impedance", "阻抗"),
    Level => ("Level", "电平"),
    Display => ("DISPLAY", "显示"),
    Start => ("START", "起始频率"),
    Stop => ("STOP", "终止频率"),
    Center => ("CENTER", "中心频率"),
    Span => ("SPAN", "频宽"),
    Points => ("POINTS", "点数"),
    MinimumSpan => ("Minimum span", "最小频宽"),
    PointsHelp => (
        "Returned samples including both endpoints. The KC901V command uses a count one less than the requested samples. Single-frequency continuous mode is not a finite sweep.",
        "包含两个端点的返回采样点数。KC901V 命令中的点数会减一。单频连续测量不属于有限扫描。"
    ),
    Rbw => ("RBW", "分辨率带宽"),
    RbwDefaultHelp => ("No selection uses the instrument's current RBW.", "不选带宽时，沿用仪器当前设置。"),
    RefLevel => ("REF LEVEL", "参考电平"),
    Calibration => ("CAL", "校准"),
    CalOn => ("Enabled", "开启"),
    CalOff => ("Disabled", "关闭"),
    CalSys => ("System", "系统"),
    CalUser => ("User", "用户"),
    Run => ("RUN", "运行"),
    StopSweep => ("STOP", "停止"),
    RunForDisplay => ("Run a sweep for this display", "请运行扫描以更新此视图"),
    LogX => ("LOG X", "X 对数"),
    LogXHelp => (
        "Base-10 frequency axis. Only positive frequencies can be shown. Changes the display, not sweep sampling. Y remains linear.",
        "以 10 为底的频率轴，仅显示正频率。只改变显示，不改变扫描采样。Y 轴保持线性。"
    ),
    AllTracesHidden => ("All traces hidden", "所有曲线已隐藏"),
    NoPositiveData => ("No data at positive frequencies", "没有正频率数据"),
    NoData => ("No data", "暂无数据"),
    HideTrace => ("Click to hide this trace", "点击隐藏此曲线"),
    ShowTrace => ("Click to show this trace", "点击显示此曲线"),
    ExportS1p => ("Export .s1p", "导出 .s1p"),
    ExportBusy => ("Export in progress", "正在导出"),
    ExportSaved => ("Saved", "已保存"),
    ExportCancelled => ("Export cancelled", "已取消导出"),
    ExportFailed => ("Export failed", "导出失败"),
    ExportNeedsComplex => (
        "Run Phase, Smith or Impedance to capture complex S11 data.",
        "请在相位、史密斯图或阻抗视图运行扫描，以获取完整复数 S11 数据。"
    ),
    ExportSnapshot => ("Last completed sweep", "最近完成的扫描"),
    ExportHelp => (
        "Exports all samples from the last completed sweep as Hz / RI / 50 ohms. Zoom, LOG X and hidden traces do not change the file. Source calibration is unchanged. Two-port export requires four complete CSVs via CLI export s2p.",
        "导出最近完成扫描的全部采样点，格式为 Hz / RI / 50 欧姆。缩放、X 对数和曲线隐藏不影响文件，不改变原有校准。两端口导出需通过 CLI export s2p 提供四份完整 CSV。"
    ),
    ReplaceFile => ("Replace existing file?", "替换已有文件？"),
    WrongExportExtension => ("Choose a .s1p destination", "请选择 .s1p 目标文件"),
}

/// Typed UI messages update immediately when switching languages. Raw
/// device diagnostics stay intact so error codes remain searchable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusMessage {
    Text(Text),
    Detail(String),
}

impl StatusMessage {
    pub fn text(&self, language: Language) -> &str {
        match self {
            Self::Text(key) => language.text(*key),
            Self::Detail(detail) => detail,
        }
    }
}

impl From<String> for StatusMessage {
    fn from(value: String) -> Self {
        Self::Detail(value)
    }
}

/// Widgets obtain the same per-frame language without owning settings.
pub fn set_language(ctx: &egui::Context, language: Language) {
    ctx.data_mut(|data| data.insert_temp(egui::Id::new("ui_language"), language));
}

pub fn language(ctx: &egui::Context) -> Language {
    ctx.data(|data| {
        data.get_temp(egui::Id::new("ui_language"))
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_complete_and_diagnostics_keep_their_details() {
        for language in Language::ALL {
            assert!(!language.label().is_empty());
            for &key in Text::ALL {
                assert!(!language.text(key).is_empty(), "{language:?}: {key:?}");
            }
            let detail = StatusMessage::from("device error packet: err_par5".to_string());
            assert_eq!(detail.text(language), "device error packet: err_par5");
        }
        let message = StatusMessage::Text(Text::Connected);
        assert_eq!(message.text(Language::English), "Connected");
        assert_eq!(message.text(Language::SimplifiedChinese), "已连接");
    }

    #[test]
    fn widget_language_is_context_local_and_defaults_to_english() {
        let first = egui::Context::default();
        let second = egui::Context::default();
        assert_eq!(language(&first), Language::English);
        set_language(&first, Language::SimplifiedChinese);
        assert_eq!(language(&first), Language::SimplifiedChinese);
        assert_eq!(language(&second), Language::English);
        set_language(&first, Language::English);
        assert_eq!(language(&first), Language::English);
    }
}
