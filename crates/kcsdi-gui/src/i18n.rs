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
    #[cfg(test)]
    pub const ALL: [Self; 2] = [Self::English, Self::SimplifiedChinese];

    /// Native language names keep the selector usable in either language.
    pub fn label(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
        }
    }

    fn from_locale(locale: &str) -> Self {
        let primary = locale
            .trim()
            .split(['-', '_', '.', '@'])
            .next()
            .unwrap_or("");
        if primary.eq_ignore_ascii_case("zh") {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }
}

/// Store the user's choice separately from the resolved display language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LanguagePreference {
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
    #[default]
    #[serde(rename = "system", other)]
    System,
}

impl LanguagePreference {
    pub const ALL: [Self; 3] = [Self::System, Self::English, Self::SimplifiedChinese];

    pub fn label(self, language: Language) -> &'static str {
        match self {
            Self::System => language.text(Text::FollowSystem),
            Self::English => Language::English.label(),
            Self::SimplifiedChinese => Language::SimplifiedChinese.label(),
        }
    }

    /// Resolve at startup and when the user changes the preference.
    pub fn resolve(self) -> Language {
        let locale = (self == Self::System)
            .then(sys_locale::get_locale)
            .flatten();
        self.resolve_locale(locale.as_deref())
    }

    fn resolve_locale(self, locale: Option<&str>) -> Language {
        match self {
            Self::System => Language::from_locale(locale.unwrap_or("")),
            Self::English => Language::English,
            Self::SimplifiedChinese => Language::SimplifiedChinese,
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
    Measurements => ("VNA + SA", "测量"),
    RfSource => ("RF source", "射频源"),
    AfSource => ("AF source", "音频源"),
    SourceFrequency => ("Frequency", "频率"),
    SourceAmplitude => ("Amplitude", "幅度"),
    SourcePort => ("Output port", "输出端口"),
    SourcePort1 => ("Port 1", "端口 1"),
    SourcePort2 => ("Port 2", "端口 2"),
    SourceAfOut => ("AF out", "音频输出"),
    SourceModulation => ("Modulation", "调制"),
    SourceModFrequency => ("Modulation frequency", "调制频率"),
    SourceDepth => ("Depth", "调制深度"),
    SourceNoModulation => ("No modulation", "无调制"),
    SourceStart => ("Start output", "启动输出"),
    SourceApplying => ("Applying", "正在应用"),
    SourceNotStarted => ("Not started", "尚未启动"),
    SourceRequested => ("Output requested", "已请求输出"),
    SourceStopSent => ("Stop sent", "已发送停止"),
    SourceUnknown => ("Output unknown", "输出状态未知"),
    SourceAboveMaximum => ("Amplitude limited by the instrument maximum.", "幅度受仪器上限限制。"),
    SourceBelowMinimum => ("Amplitude limited by the instrument minimum.", "幅度受仪器下限限制。"),
    SourceApplyHelp => ("Edits take effect when you press Apply.", "更改参数后点击应用生效。"),
    SourceStopForHealth => ("Stop source output before refreshing readings.", "停止信号源后再刷新读数。"),
    SourceStopBeforeMeasure => ("Stop source output before measuring.", "停止信号源后再进行测量。"),
    CalibrationWizard => ("Calibration", "校准向导"),
    CalibrationS11System => ("S11 system", "S11 系统校准"),
    CalibrationS21System => ("S21 system", "S21 系统校准"),
    CalibrationS11User => ("S11 user", "S11 用户校准"),
    CalibrationS21User => ("S21 user", "S21 用户校准"),
    CalibrationSystemRange => ("Uses the instrument's system calibration range.", "使用仪器的系统校准范围。"),
    CalibrationMeasurementsOnly => ("User calibration requires measurement mode.", "用户校准需要测量模式。"),
    CalibrationContinuousOnly => ("User calibration requires a continuous frequency range, not a list.", "用户校准需要连续频率范围，不支持频率列表。"),
    CalibrationSelectTrace => ("Select a trace with the matching S11 or S21 mode.", "请选择对应的 S11 或 S21 轨迹。"),
    CalibrationWriteWarning => ("This writes calibration data to the instrument and may replace the existing correction.", "此操作会写入仪器校准数据，可能替换现有修正。"),
    CalibrationKitHelp => ("Use standards that match the instrument's stored kit settings.", "请使用与仪器内校准件设置一致的标准件。"),
    CalibrationConsent => ("I agree to write calibration data.", "我确认写入校准数据。"),
    CalibrationStart => ("Start calibration", "开始校准"),
    CalibrationStarting => ("Starting calibration", "正在启动校准"),
    CalibrationWarming => ("Waiting for the instrument", "等待仪器准备"),
    CalibrationShort => ("Short on port 1", "端口 1 接短路标准件"),
    CalibrationOpen => ("Open on port 1", "端口 1 接开路标准件"),
    CalibrationLoad => ("Load on port 1", "端口 1 接负载标准件"),
    CalibrationThrough => ("Connect ports 1 and 2", "直通连接端口 1 和端口 2"),
    CalibrationConnectHelp => ("Make the connection above, then press Next.", "按上方提示完成连接，然后点击下一步。"),
    CalibrationMeasuring => ("Measuring standard", "正在测量标准件"),
    CalibrationProcessing => ("Calculating correction", "正在计算修正"),
    CalibrationSaving => ("Saving calibration", "正在保存校准"),
    CalibrationCompleted => ("Calibration completed", "校准完成"),
    CalibrationCancelled => ("Calibration cancelled", "校准已取消"),
    CalibrationUnknown => ("Calibration result unknown", "校准结果未知"),
    CalibrationCancelling => ("Cancelling calibration", "正在取消校准"),
    CalibrationChanged => ("Correction data may have changed. Start a new calibration to try again.", "修正数据可能已经改变。如需重试，请重新开始校准。"),
    CalibrationReacquire => ("Select the matching correction and run a new sweep. Previous traces remain available for export.", "选择对应的修正后重新扫频。原有轨迹仍可导出。"),
    CalibrationNew => ("New calibration", "重新校准"),
    CalibrationStopSource => ("Stop source output before calibration.", "停止信号源后再进行校准。"),
    CalibrationBusy => ("Finish or cancel calibration first.", "请先完成或取消校准。"),
    Next => ("Next", "下一步"),
    Back => ("Back", "返回"),
    Close => ("Close", "关闭"),
    CalibrationStepDone => ("Done", "已完成"),
    CalibrationStepCurrent => ("Current", "当前"),
    CalibrationStepPending => ("Pending", "待完成"),
    CalibrationStepInterrupted => ("Not completed", "未完成"),
    TraceList => ("TRACE LIST", "轨迹列表"),
    TraceSettings => ("Trace settings", "轨迹设置"),
    FrequencyRangeTab => ("FREQUENCY RANGE", "频率范围"),
    FrequencyRange => ("Range", "范围"),
    FrequencyList => ("Frequency list", "频率列表"),
    FrequencyListTab => ("List", "列表"),
    RangeTo => ("to", "至"),
    FrequencyListHelp => ("Enter 3 to 1001 frequencies. Apply sorts them and keeps repeated points.", "输入 3 至 1001 个频率。应用时按频率排序，保留重复点。"),
    ImportFrequencyList => ("Import list", "导入列表"),
    FrequencyTemplate => ("Save template", "保存模板"),
    FrequencyFilePending => ("Finish or cancel the file dialog to continue.", "完成或取消文件对话框后继续。"),
    EditFrequencyList => ("Edit list", "编辑列表"),
    Unit => ("Unit", "单位"),
    Apply => ("Apply", "应用"),
    VisibleTracesRun => ("Run acquires all visible traces", "运行时采集所有可见轨迹"),
    AddTrace => ("Add trace", "添加轨迹"),
    Color => ("Color", "颜色"),
    LineWidth => ("Line width", "线宽"),
    LocalOscillator => ("Local oscillator", "本振"),
    HighLo => ("High side", "高侧"),
    LowLo => ("Low side", "低侧"),
    NoTraces => ("Add a trace to start measuring.", "添加轨迹以开始测量。"),
    TraceLimit => ("At most 10 traces", "最多 10 条轨迹"),
    Connection => ("Connection", "连接设置"),
    Show => ("Show", "显示"),
    Hide => ("Hide", "隐藏"),
    LocalDevices => ("Local devices", "本地设备"),
    AddDevice => ("Add device", "添加设备"),
    EditDevice => ("Edit device", "编辑设备"),
    DeviceName => ("Name", "名称"),
    NoSavedDevices => ("Add a device to get started.", "添加设备以开始使用。"),
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
    HealthStatus => ("Readings", "读数"),
    HealthUnavailable => ("No readings", "暂无读数"),
    HealthUpdating => ("Updating", "正在更新"),
    HealthStale => ("Stale", "未更新"),
    HealthRefreshFailed => ("Refresh failed", "刷新失败"),
    HealthSecondsAgo => ("s ago", "秒前"),
    HealthMinutesAgo => ("min ago", "分钟前"),
    HealthHoursAgo => ("h ago", "小时前"),
    HealthDaysAgo => ("d ago", "天前"),
    DeviceInfoUnavailable => ("Connect an instrument to view its details.", "连接仪器后查看设备信息。"),
    Refresh => ("Refresh", "刷新"),
    ProfileNameRequired => ("Enter a name.", "请输入名称。"),
    ProfileHostRequired => ("Enter a host name or IP address.", "请输入主机名或 IP 地址。"),
    Network => ("TCP", "TCP"),
    Serial => ("Serial", "串口"),
    SerialPath => ("Serial port path", "串口路径"),
    SerialPathRequired => ("Enter a port path without control characters.", "请输入不含控制字符的串口路径。"),
    PortLookupFailed => ("Could not list ports", "无法读取串口列表"),
    NoSerialPorts => ("No ports listed. You can enter a path.", "未列出串口，可手动输入路径。"),
    DiscoverDevices => ("Discover", "发现设备"),
    DiscoveryScan => ("Scan", "查找"),
    DiscoveryHelp => ("Find advertised KC901V devices. Results expire after 30 seconds.", "查找广播中的 KC901V。结果保留 30 秒。"),
    DiscoveryEmpty => ("No current results. You can add a device manually.", "暂无有效结果，可手动添加设备。"),
    DiscoveryFailed => ("Discovery failed", "设备查找失败"),
    AddDiscovered => ("Add", "添加"),
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
    AnalysisTarget => ("Target", "目标曲线"),
    AnalysisCurrent => ("Current", "当前曲线"),
    AnalysisTargetUnavailable => ("No completed data for this target.", "此目标尚无完整数据。"),
    AnalysisCenter => ("CENTER", "移至中心"),
    AnalysisCenterAtMarker => ("C=M", "中心设为标记"),
    AnalysisCenterAtMarkerHelp => ("Center the sweep at this marker. Span may shrink near the frequency limits.", "以标记频率为扫描中心，接近频率边界时缩小频宽。"),
    AnalysisAutoPeak => ("Track peak", "跟踪峰值"),
    AnalysisDeltaReference => ("Delta reference", "差值参考"),
    AnalysisReference => ("REF", "参考"),
    Language => ("Language", "语言"),
    FollowSystem => ("Follow system", "跟随系统"),
    Host => ("Host", "主机"),
    Port => ("Port", "端口"),
    Connect => ("Connect", "连接"),
    Disconnect => ("Disconnect", "断开"),
    Disconnected => ("Disconnected", "未连接"),
    Connecting => ("Connecting", "连接中"),
    Disconnecting => ("Disconnecting", "正在断开"),
    Stopping => ("Stopping", "正在停止"),
    Closing => ("Closing", "正在退出"),
    SweepProgress => ("Scanning", "正在扫描"),
    Connected => ("Connected", "已连接"),
    Error => ("Error", "错误"),
    Firmware => ("sw", "固件"),
    ExternalPower => ("ext", "外部"),
    Battery => ("bat", "电池"),
    Spectrum => ("SPEC", "频谱"),
    Phase => ("Phase", "相位"),
    ReturnLoss => ("Return Loss", "回波损耗"),
    Loss => ("Loss", "损耗"),
    GroupDelay => ("Group delay", "群时延"),
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
    RefLevel => ("REF LEVEL", "参考电平"),
    Calibration => ("CAL", "校准"),
    CalOn => ("Enabled", "开启"),
    CalOff => ("Disabled", "关闭"),
    CalSys => ("System", "系统"),
    CalUser => ("User", "用户"),
    Run => ("RUN", "运行"),
    RunAndSave => ("RUN AND SAVE", "运行并保存"),
    RunSettings => ("Run settings", "运行设置"),
    RunInterval => ("Wait", "等待"),
    RunIntervalHelp => ("Wait after each complete pass. Recording finishes before the wait.", "每轮完成后再等待。启用记录时先保存文件。"),
    RunSeconds => ("seconds", "秒"),
    RunMinutes => ("minutes", "分钟"),
    RunHours => ("hours", "小时"),
    RunDays => ("days", "天"),
    RunRecording => ("Save each complete pass", "保存每轮完整测量"),
    RunDirectory => ("Recording folder", "记录文件夹"),
    RunChooseDirectory => ("Choose folder", "选择文件夹"),
    RunNoDirectory => ("No folder selected", "未选择文件夹"),
    RunRetention => ("Keep", "保留"),
    RunKeepAll => ("All", "全部"),
    RunKeepLast => ("Latest", "最近"),
    RunFiles => ("files", "个文件"),
    RunRetentionHelp => ("Each run uses a new subfolder.", "每次运行使用新子文件夹。"),
    RunPruneHelp => ("The limit deletes older files from this run only.", "数量限制只删除本次运行的旧文件。"),
    RunRestartHelp => ("Changes restart an active run.", "更改设置会重新开始当前运行。"),
    RunShortInterval => ("A wait below 10 seconds can create many files.", "等待不足 10 秒可能产生大量文件。"),
    RunIntervalInvalid => ("Enter a wait from 0 to 365 days.", "请输入零至 365 天的等待时间。"),
    RunDirectoryRequired => ("Choose an absolute recording folder path.", "请选择使用绝对路径的记录文件夹。"),
    RunRetentionInvalid => ("Keep between 1 and 256 files.", "保留数量须在 1 至 256 之间。"),
    RunSaving => ("Saving", "正在保存"),
    RunWaiting => ("Waiting", "等待中"),
    RunSaved => ("Saved pass", "已保存轮次"),
    RunFilePending => ("Close the folder dialog to continue.", "关闭文件夹对话框后继续。"),
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
    Export => ("Export", "导出"),
    ExportFormat => ("Format", "格式"),
    ExportScope => ("Scope", "范围"),
    ExportSelected => ("Selected trace", "当前轨迹"),
    ExportVisible => ("Visible traces", "可见轨迹"),
    ExportPoints => ("points", "点"),
    ExportTraces => ("traces", "条轨迹"),
    ExportMissing => ("Missing completed data", "缺少完整数据"),
    ExportEmpty => ("No traces to export", "没有可导出的轨迹"),
    ExportBusy => ("Export in progress", "正在导出"),
    ExportSaved => ("Saved", "已保存"),
    ExportCancelled => ("Export cancelled", "已取消导出"),
    ExportCancelling => ("Cancelling export", "正在取消导出"),
    ExportCancelHelp => ("Close the file dialog to finish cancelling.", "关闭文件对话框即可完成取消。"),
    ExportFailed => ("Export failed", "导出失败"),
    ExportNeedsComplex => (
        "Select an S11 Phase, Smith or Impedance trace and run a sweep.",
        "选择 S11 相位、史密斯图或阻抗轨迹并运行扫描。"
    ),
    ExportHelp => (
        "Exports complete measurements frozen on click, including hidden components. Hold, Max and Min are excluded.",
        "导出点击时最近完成的测量数据，包含隐藏分量，不含保持和最大值、最小值包络。"
    ),
    ReplaceFile => ("Replace existing file?", "替换已有文件？"),
    WrongExportExtension => ("Required file extension", "所需文件扩展名"),
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
    fn system_locale_supports_bcp47_and_posix_without_prefix_false_matches() {
        for locale in [
            "zh",
            "zh-CN",
            "zh_Hans_CN.UTF-8",
            "zh_TW",
            "ZH-hant-HK",
            " zh_CN@variant ",
        ] {
            assert_eq!(
                LanguagePreference::System.resolve_locale(Some(locale)),
                Language::SimplifiedChinese
            );
        }
        for locale in [
            "en",
            "en_US.UTF-8",
            "en-GB",
            "de_DE",
            "ja-JP",
            "C",
            "POSIX",
            "",
            "zhuang",
            "zhanything",
        ] {
            assert_eq!(
                LanguagePreference::System.resolve_locale(Some(locale)),
                Language::English
            );
        }
        assert_eq!(
            LanguagePreference::System.resolve_locale(None),
            Language::English
        );
    }

    #[test]
    fn explicit_choices_override_the_system_and_can_return_to_it() {
        for locale in [Some("zh_CN.UTF-8"), Some("en-US"), Some("de-DE"), None] {
            assert_eq!(
                LanguagePreference::English.resolve_locale(locale),
                Language::English
            );
            assert_eq!(
                LanguagePreference::SimplifiedChinese.resolve_locale(locale),
                Language::SimplifiedChinese
            );
        }
        let system = LanguagePreference::default();
        assert_eq!(system.resolve_locale(Some("en-US")), Language::English);
        assert_eq!(
            system.resolve_locale(Some("zh_CN.UTF-8")),
            Language::SimplifiedChinese
        );
    }

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
