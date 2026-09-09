// 版本与更新说明（“关于”弹窗展示，发新版时在 CHANGELOG 数组顶部加一项）。
//
// 版本号需与 package.json / tauri.conf.json / Cargo.toml 保持一致，
// 发版时统一修改（项目规范：所有版本号同步更新）。

/** 当前版本号（Tauri 环境以 tauri.conf.json 的 version 为准，此处为浏览器环境回退值） */
export const APP_VERSION = "1.1.0";

/** 更新说明：按版本从新到旧排列，每项含版本号与本次改动要点 */
export interface ChangelogEntry {
    version: string;
    notes: string[];
}

export const CHANGELOG: ChangelogEntry[] = [
    {
        version: "1.1.0",
        notes: [
            "新增「Lua 脚本回报」模式：用 Lua 脚本按委托字段自定义确认/成交/拒单回报（详见 docs/Lua脚本回报编写说明.md）",
            "Lua 脚本编辑器支持语法高亮、行号与即时语法检查，保存时后端强制校验",
        ],
    },
    {
        version: "1.0.0",
        notes: ["支持深交所Binary协议文档V1.32版，上海竞价文档V0.54版，上海新债券文档V1.93版"],
    },
];

/** 取最近一个版本的更新说明（“关于”弹窗里展示） */
export function latestNotes(): ChangelogEntry {
    return CHANGELOG[0];
}
