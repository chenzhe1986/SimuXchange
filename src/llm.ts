// llm.ts —— "AI 生成脚本"的大模型调用（纯前端实现，不经模拟引擎后端）。
//
// 链路：PlatformEditor 的自然语言输入 → 本模块拼 [系统提示词, 用户需求] 两条
// 消息（单轮、每次独立，不携带历史）→ 调 OpenAI 兼容的 chat/completions 接口
// → 解析模型返回的 JSON（script / unsupported / clarify 三种结果）。
//
// CORS 说明：模型接口不返回 CORS 头，WebView/浏览器里的 fetch 会被浏览器
// 拦截，所以桌面端经 Tauri 壳的 llm_chat command 代发（壳只透传一次 HTTPS
// 请求，模拟引擎 simx-core / Linux 后端完全不参与）；纯浏览器模式（npm run
// dev 连远程后端）无壳可借，被 CORS 拦截时会给出明确提示。

import { buildUserMessage, SYSTEM_PROMPT } from "./llmPrompt";
import type { LlmCfg } from "./types";

// 模型接口配置来自 simx.config.json 的 llm 字段（桌面端经 Tauri 的
// get_app_config 读取，见 loadLlmConfig）：换模型/服务商只改配置文件，
// 无需重新打包。纯浏览器模式（npm run dev 连远程后端）没有配置文件可读，
// 回退下面这份内置默认值——修改它时请与 simx-core/src/config.rs 的
// LlmConfig::default 保持一致（两处只在“无配置文件”时二选一使用）。
const DEFAULT_LLM_CONFIG: LlmCfg = {
    /** 接口根地址（到 /v1 为止，代码里自己拼 /chat/completions） */
    baseUrl: "https://developer.amd.com.cn/radeon/v1",
    /** 服务商鉴权 key */
    apiKey: "rc-b824e20605e5c9f4a9ce7d3155f93fd00590b0958be59c2f",
    /** 模型 id */
    model: "MiMo-V2.6-Flash",
    /** 采样温度：脚本生成要稳定，取低值 */
    temperature: 0.2,
    /** 单次输出的 token 上限（脚本很短，8K 足够；该服务商字段名为 max_tokens） */
    maxTokens: 8192,
};

const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** 读取模型配置：桌面端取 simx.config.json 的 llm 字段；纯浏览器
 *  （无 Tauri 壳）读不到配置文件，用内置默认值兜底 */
async function loadLlmConfig(): Promise<LlmCfg> {
    if (!isTauri) return DEFAULT_LLM_CONFIG;
    try {
        const { invoke } = await import("@tauri-apps/api/core");
        // 后端结构 AppConfig 序列化为 camelCase，llm 字段恒存在（缺省为默认值）
        const cfg = (await invoke("get_app_config")) as { llm?: LlmCfg };
        if (cfg?.llm) return cfg.llm;
    } catch {
        // 读取失败按内置默认值处理，不阻塞生成
    }
    return DEFAULT_LLM_CONFIG;
}

/** 生成结果的三种形态（与系统提示词第五节的输出协议一一对应） */
export type GenerateResult =
    | { type: "script"; script: string }
    | { type: "unsupported"; reason: string }
    | { type: "clarify"; question: string };

/** 从模型返回文本里提取 JSON：容忍 ```json 围栏与前后说明文字（按第一个 {
 *  到最后一个 } 截取后再解析，覆盖绝大多数格式漂移） */
function extractJson(text: string): Record<string, unknown> {
    const start = text.indexOf("{");
    const end = text.lastIndexOf("}");
    if (start < 0 || end <= start) {
        throw new Error("模型没有返回 JSON 结果");
    }
    return JSON.parse(text.slice(start, end + 1)) as Record<string, unknown>;
}

/** OpenAI 兼容响应 → 正文 content（推理模型可能带 reasoning_content，只取正文） */
function contentOf(resp: unknown): string {
    const choices = (resp as { choices?: { message?: { content?: unknown } }[] })?.choices;
    return String(choices?.[0]?.message?.content ?? "");
}

/** 发一次 chat/completions（单轮），返回模型正文 */
async function chat(cfg: LlmCfg, messages: { role: string; content: string }[]): Promise<string> {
    // 配置来自用户可以手改的 simx.config.json：这里做边界校验，给出可操作的报错
    if (!cfg.baseUrl.trim() || !cfg.model.trim()) {
        throw new Error("未配置模型接口地址或模型名（simx.config.json 的 llm.baseUrl / llm.model）");
    }
    if (!cfg.apiKey.trim()) {
        throw new Error("未配置模型 API 密钥（simx.config.json 的 llm.apiKey）");
    }
    // 容忍地址末尾多写的斜杠（拼出来不会出现 //chat/completions）
    const url = `${cfg.baseUrl.replace(/\/+$/, "")}/chat/completions`;
    const body = {
        model: cfg.model,
        messages,
        temperature: cfg.temperature,
        max_tokens: cfg.maxTokens,
    };
    let resp: unknown;
    if (isTauri) {
        // 桌面端：经 Tauri 壳转发（绕 CORS；壳只透传，见 src-tauri 的 llm_chat）
        const { invoke } = await import("@tauri-apps/api/core");
        resp = await invoke<unknown>("llm_chat", {
            url,
            headers: {
                Authorization: `Bearer ${cfg.apiKey}`,
                "Content-Type": "application/json",
            },
            body,
        });
    } else {
        // 纯浏览器（npm run dev 连远程后端）：直连，若服务端无 CORS 头会失败
        const http = await fetch(url, {
            method: "POST",
            headers: {
                Authorization: `Bearer ${cfg.apiKey}`,
                "Content-Type": "application/json",
            },
            body: JSON.stringify(body),
        });
        if (!http.ok) {
            throw new Error(`模型接口返回 ${http.status}：${(await http.text()).slice(0, 300)}`);
        }
        resp = await http.json();
    }
    const content = contentOf(resp);
    if (!content) {
        throw new Error("模型没有返回正文（推理占用了全部输出？请重试）");
    }
    return content;
}

/** 自然语言 → Lua 回报脚本（或"不支持"/"需要补充信息"两种提示） */
export async function generateLuaScript(input: string): Promise<GenerateResult> {
    // 每次点击生成时读取一次配置：配置文件改动在程序重启后生效
    const cfg = await loadLlmConfig();
    const raw = await chat(cfg, [
        { role: "system", content: SYSTEM_PROMPT },
        { role: "user", content: buildUserMessage(input) },
    ]);
    let obj: Record<string, unknown>;
    try {
        obj = extractJson(raw);
    } catch {
        // 解析失败时把原文带给上层展示，用户可手动拷贝脚本救急
        throw new Error(`模型返回内容无法解析：\n${raw.slice(0, 2000)}`);
    }
    const type = String(obj.type ?? "");
    if (type === "script" && typeof obj.script === "string" && obj.script.trim()) {
        return { type: "script", script: obj.script };
    }
    if (type === "unsupported") {
        return { type: "unsupported", reason: String(obj.reason ?? "该需求超出脚本能力范围") };
    }
    if (type === "clarify") {
        return { type: "clarify", question: String(obj.question ?? "需求信息不足，请补充描述") };
    }
    throw new Error(`模型返回的结果类型无法识别：${type || "(空)"}`);
}
