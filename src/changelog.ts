// 版本与更新说明（“关于”弹窗展示，发新版时在 CHANGELOG 数组顶部加一项）。
//
// 版本号需与 package.json / tauri.conf.json / Cargo.toml 保持一致，
// 发版时统一修改（项目规范：所有版本号同步更新）。

/** 当前版本号（Tauri 环境以 tauri.conf.json 的 version 为准，此处为浏览器环境回退值） */
export const APP_VERSION = "1.0.0";

/** 更新说明：按版本从新到旧排列，每项含版本号与本次改动要点 */
export interface ChangelogEntry {
    version: string;
    notes: string[];
}

export const CHANGELOG: ChangelogEntry[] = [
    {
        version: "1.0.0",
        notes: ["初始版本，支持深圳竞价、上海竞价、上海新债券平台"],
    },
];

/** 取最近一个版本的更新说明（“关于”弹窗里展示） */
export function latestNotes(): ChangelogEntry {
    return CHANGELOG[0];
}
