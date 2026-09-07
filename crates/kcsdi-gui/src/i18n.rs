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
    Sweep => ("SWEEP", "扫描"),
    Display => ("DISPLAY", "显示"),
    Receiver => ("RECEIVER", "接收机"),
    Start => ("START", "起始频率"),
    Stop => ("STOP", "终止频率"),
    Center => ("CENTER", "中心频率"),
    Span => ("SPAN", "频宽"),
    Points => ("POINTS", "点数"),
    FrequencyRange => ("Range", "频率范围"),
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
