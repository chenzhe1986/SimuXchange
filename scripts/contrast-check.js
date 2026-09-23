// contrast-check.js —— 设计新主题色时用的 WCAG 对比度验证脚本（临时工具，验证后可删）
// 用法：node scripts/contrast-check.js
// 目标：正文 ≥ 7、次要文字 ≥ 4.5、提示文字 ≥ 4.0、按钮文字（白/深）≥ 4.5、
//       主题色本身 ≥ 3、状态色（绿/黄/红）≥ 4.0。

const THEMES = {
    // 优雅白（浅色）
    light: {
        bg: "#f3f5f9", panel: "#ffffff", bgSoft: "#e9edf4",
        text: "#1c2632", dim: "#5b6d82", faint: "#8c99ab",
        accent: "#2563eb", onAccent: "#ffffff",
        green: "#047857", amber: "#ad5700", red: "#b91c1c",
        btnGreen: "#0e9a6e", btnGreenHover: "#0c8c60",
    },
    // 经典黑（深色）
    dark: {
        bg: "#0b0f16", panel: "#181c29", bgSoft: "#121824",
        text: "#e6ebf2", dim: "#a3afc0", faint: "#7e8fa8",
        accent: "#6fa8ff", onAccent: "#0a1526",
        green: "#45d6a0", amber: "#f2c14f", red: "#f27f7f",
        btnGreen: "#2bbf8a", btnGreenHover: "#35cf97",
    },
    // 现代蓝
    blue: {
        bg: "#07162e", panel: "#0e2c4f", bgSoft: "#0b2240",
        text: "#ddebfb", dim: "#a3c3e8", faint: "#8aa8c9",
        accent: "#45c7f8", onAccent: "#06313f",
        green: "#45d6a0", amber: "#f2c14f", red: "#f78686",
        btnGreen: "#0ea5a6", btnGreenHover: "#0bb8b2",
    },
    // 深空灰
    gray: {
        bg: "#17191e", panel: "#23272e", bgSoft: "#1c2127",
        text: "#e9ecef", dim: "#a6aeb9", faint: "#8b96a5",
        accent: "#2dd4bf", onAccent: "#0a2e28",
        green: "#34d399", amber: "#f2c14f", red: "#f07d8f",
        btnGreen: "#3bbfa5", btnGreenHover: "#46cdb1",
    },
    // 极光紫
    purple: {
        bg: "#100b20", panel: "#1d1542", bgSoft: "#170f2e",
        text: "#efeaff", dim: "#b3a7d8", faint: "#978bc0",
        accent: "#c08bfc", onAccent: "#1d0b33",
        green: "#4fd6b1", amber: "#f2c14f", red: "#f8938f",
        btnGreen: "#7f6fd9", btnGreenHover: "#8d7de0",
    },
};

function hex2rgb(h) {
    h = String(h).replace("#", "");
    h = h.length === 3 ? h.split("").map((c) => c + c).join("") : h;
    return [parseInt(h.slice(0, 2), 16), parseInt(h.slice(2, 4), 16), parseInt(h.slice(4, 6), 16)];
}
function lum(c) {
    const [r, g, b] = hex2rgb(c).map((v) => {
        v /= 255;
        return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function ratio(a, b) {
    const la = lum(a), lb = lum(b);
    return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

for (const [name, t] of Object.entries(THEMES)) {
    console.log(`\n===== ${name} =====`);
    const checks = [
        ["正文 text/bg", [t.text, t.bg], 4.5],
        ["正文 text/panel", [t.text, t.panel], 4.5],
        ["次要 dim/bg", [t.dim, t.bg], 4.5],
        ["次要 dim/panel", [t.dim, t.panel], 4.5],
        ["提示 faint/bg", [t.faint, t.bg], 4.0],
        ["提示 faint/panel", [t.faint, t.panel], 4.0],
        ["提示 faint/bgSoft", [t.faint, t.bgSoft], 4.0],
        ["主色 accent/panel(UI)", [t.accent, t.panel], 3.0],
        ["按钮字 onAccent/accent", [t.onAccent, t.accent], 4.5],
        ["绿字/bg", [t.green, t.bg], 4.0],
        ["绿字/panel", [t.green, t.panel], 4.0],
        ["黄字/panel", [t.amber, t.panel], 4.0],
        ["红字/panel", [t.red, t.panel], 4.0],
        ["按钮白/btnGreen", ["#ffffff", t.btnGreen], 4.0],
        ["边框 border?"]
    ];
    for (const [label, pair, min] of checks) {
        if (!pair) continue;
        const v = ratio(pair[0], pair[1]);
        const ok = v >= min;
        console.log(`  ${ok ? "OK  " : "FAIL"} ${v.toFixed(2)}:1 (min ${min})  ${label}`);
    }
}