// 版本与更新说明（“关于”弹窗展示，发新版时在 CHANGELOG 数组顶部加一项）。
//
// 版本号需与 package.json / tauri.conf.json / src-tauri/Cargo.toml 保持一致，
// 发版时统一修改（项目规范：所有版本号同步更新）；各工具 crate
// （simx-core / simx-server / simx-update-server / simx-cli）的 Cargo.toml
// 版本也随发版同步——simx-server 的 --update-url 检查用该版本号与服务器
// 清单比较，simx-cli 的 --version 直接输出它，不同步会误报有新版本。

/** 当前版本号（Tauri 环境以 tauri.conf.json 的 version 为准，此处为浏览器环境回退值） */
export const APP_VERSION = "2.0.0";

/** 开源项目主页（“关于”弹窗里的链接，点击在系统浏览器中打开） */
export const GITHUB_URL = "https://github.com/chenzhe1986/SimuXchange";
export const GITEE_URL = "https://gitee.com/chenzhe1986/SimuXchange";

/** 更新说明：按版本从新到旧排列，每项含版本号与本次改动要点 */
export interface ChangelogEntry {
    version: string;
    notes: string[];
}

export const CHANGELOG: ChangelogEntry[] = [
    {
        version: "2.0.0",
        notes: [
            "新增 Lua 脚本配置自动回报模式：用 Lua 脚本按委托字段自定义确认/成交/拒单回报，编辑器带语法高亮、即时校验与 AI 生成脚本（详见 docs/Lua脚本回报编写说明.md）",
            "新增网关分组、标签、备注等功能，优化界面布局",
            "增加命令行工具 simx-cli，方便进行自动化测试（详见 docs/CLI工具使用手册.md）",
        ],
    },
    {
        version: "1.4.0",
        notes: [
            "深圳非交易处理平台（平台类型 3）新增「ETF清单文件路径」配置：填写存放 ETF 申购赎回清单（pcf_<代码>_<日期>.xml）的目录后，收到 ETF 实时申购赎回委托时自动按代码查找日期最新的清单文件，把全部成份股明细（每只成份券的交付数量与现金替代金额，申购/赎回分别取申购/赎回替代金额）填入订单执行报告",
            "目录留空 = 不启用（成份股数为 0）；目录不可用或找不到对应代码的清单文件时同样按 0 只成份股回复，并在运行日志推送告警，便于定位清单数据缺失",
            "清单查找与解析带缓存：目录内容变化（新增/更新清单文件）后下一笔委托即时生效；目录无变化时不重复遍历目录、不重复解析文件，清单目录文件很多也不影响收单性能",
            "收发报文弹窗对 ETF 申赎执行报告的成份股明细按 Security[i].UnderlyingSecurityID / DeliveryQty / SubstCash 等字段逐条结构化解析展示，与原始字节同屏对照",
        ],
    },
    {
        version: "1.3.0",
        notes: [
            "新增 simx-cli 命令行控制工具：连接已运行的 simx-server，用一条条命令完成查状态/查订单、热更新回报策略、手动回报订单等操作，方便自动化测试脚本调用（详见 docs/CLI工具使用手册.md）",
            "simx-cli 支持登录（login）把服务器地址保存到用户主目录 .simx-cli.json，之后所有命令免 --url；单条命令可临时 --url 覆盖，logout 清除",
            "simx-cli 定位网关/平台时按 id 优先；名称允许重复，命中多个时明确报错并列出候选 id（不再默默取第一个）",
            "新增一键打包脚本 pack-all.ps1：一条命令产出桌面客户端、Windows/Linux 双版 CLI 与 Linux 后端三件套，汇总到 dist\\windows 与 dist\\linux（见构建手册 6.1）",
        ],
    },
    {
        version: "1.2.0",
        notes: [
            "五套配色主题全面重设计：文字对比度全部达到 WCAG AA 标准，弱提示文字不再灰到看不清",
            "字体渲染优化：Windows 下恢复 ClearType 次像素渲染（文字更锐利），正文 14px、全界面小字/小数号统一上调",
            "输入框聚焦增加高亮描边，主题色实底按钮的按钮字色自动适配深/浅主题",
        ],
    },
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
