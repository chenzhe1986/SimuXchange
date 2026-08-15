// 后端接入层：本地（Tauri 内嵌引擎）与远程（WebSocket）共用同一套 JSON 命令协议
// 连接方式由 simx.config.json 决定，也可在界面“后端连接设置”里切换（切换时
// 经 set_app_config 写回配置文件，下次启动仍然生效）
//
// 采用“接口 + 两个实现”模式（类比 C++ 抽象基类 + 派生类）：
// Backend 接口声明能力（发命令、收事件…），LocalBackend（Tauri 进程内）
// 与 RemoteBackend（WebSocket）各自实现。App.vue 只依赖 Backend 接口，
// 切换连接模式时界面代码无需修改。

import type { AppCfg, BackendEntry } from "./types";

/** 后端命令的统一响应格式（与 simx-core api.rs 的 ok/err 对应） */
export interface CmdResp {
    ok: boolean;
    data?: unknown;
    error?: string;
}

export interface Backend {
    /** "local" | "remote" */
    readonly mode: string;
    /** 远程模式下的连接地址（界面设置弹窗回填用）；本地模式无此值 */
    readonly url?: string;
    /** 远程模式下的连接状态；本地模式恒为 true */
    isConnected(): boolean;
    /** 发送控制命令，payload 形如 { cmd: "get_snapshot", ... } */
    dispatch(payload: Record<string, unknown>): Promise<CmdResp>;
    /** 订阅引擎事件（日志等） */
    onEvent(cb: (ev: Record<string, unknown>) => void): void;
    /** 订阅连接状态变化（远程模式） */
    onStatusChange(cb: (connected: boolean) => void): void;
    /** 释放连接：切换后端或页面卸载时调用，停止监听/重连，避免旧实例残留 */
    dispose(): void;
}

/** 判断是否跑在 Tauri 里（Tauri 会往 window 上注入这个内部对象；
 *  纯浏览器里没有，据此区分两种运行环境） */
function inTauri(): boolean {
    return "__TAURI_INTERNALS__" in window;
}

/** 本地模式：直接调用 Tauri command，事件走 Tauri event。
 *  引擎就在本进程内，没有网络，所以永远“在线” */
class LocalBackend implements Backend {
    readonly mode = "local";
    /** 已注册的 Tauri 事件退订函数；dispose 时统一取消，防止切换后端后日志重复推送 */
    private unlisteners: (() => void)[] = [];

    isConnected(): boolean {
        return true;
    }

    async dispatch(payload: Record<string, unknown>): Promise<CmdResp> {
        // 动态 import：只在真正需要时才加载 Tauri API，
        // 避免纯浏览器环境下加载不存在的模块报错
        const { invoke } = await import("@tauri-apps/api/core");
        try {
            return (await invoke("dispatch", { payload })) as CmdResp;
        } catch (e) {
            return { ok: false, error: String(e) };
        }
    }

    async onEvent(cb: (ev: Record<string, unknown>) => void): Promise<void> {
        const { listen } = await import("@tauri-apps/api/event");
        // 后端 main.rs 里 emit("engine-event", ...) 推的事件在这里接收
        const unlisten = await listen("engine-event", (e) => cb(e.payload as Record<string, unknown>));
        this.unlisteners.push(unlisten);
    }

    onStatusChange(_cb: (connected: boolean) => void): void {
        // 本地引擎始终在线
    }

    dispose(): void {
        this.unlisteners.forEach((un) => un());
        this.unlisteners = [];
    }
}

/** 远程模式：WebSocket 请求-响应（带 id 关联）+ 事件推送（无 id），断线自动重连 */
class RemoteBackend implements Backend {
    readonly mode = "remote";
    readonly url: string;
    private ws: WebSocket | null = null;
    private connected = false;
    /** 请求 id 自增器：每发一条命令 +1，用于把响应对回请求 */
    private nextId = 1;
    /** 已发出、还没收到响应的请求：id → 唤醒等待者的回调 */
    private pending = new Map<number, (resp: CmdResp) => void>();
    private eventCbs: ((ev: Record<string, unknown>) => void)[] = [];
    private statusCbs: ((connected: boolean) => void)[] = [];
    /** 已销毁标记：dispose 后不再重连、不再发命令 */
    private disposed = false;

    constructor(url: string) {
        this.url = url;
        this.connect();
    }

    /** 建立 WebSocket 连接并挂好四个事件处理器；失败则安排重连 */
    private connect(): void {
        if (this.disposed) return;
        try {
            this.ws = new WebSocket(this.url);
        } catch {
            this.scheduleReconnect();
            return;
        }
        this.ws.onopen = () => this.setConnected(true);
        this.ws.onclose = () => {
            this.setConnected(false);
            // 断线了，正在等响应的请求不可能再收到回复，全部报错唤醒
            this.failAllPending("连接已断开");
            this.scheduleReconnect();
        };
        this.ws.onerror = () => this.ws?.close();
        this.ws.onmessage = (e) => {
            let msg: Record<string, unknown>;
            try {
                msg = JSON.parse(String(e.data));
            } catch {
                return;
            }
            // 有 id 且在等待表里 → 是某次请求的响应；有 event 字段 → 是推送事件
            if (typeof msg.id === "number" && this.pending.has(msg.id)) {
                const resolve = this.pending.get(msg.id)!;
                this.pending.delete(msg.id);
                resolve((msg.resp as CmdResp) ?? { ok: false, error: "空响应" });
            } else if (msg.event) {
                this.eventCbs.forEach((cb) => cb(msg));
            }
        };
    }

    /** 2 秒后重试连接（死循环式重连：连不上就一直试，服务恢复后自动接上） */
    private scheduleReconnect(): void {
        if (this.disposed) return;
        setTimeout(() => this.connect(), 2000);
    }

    /** 更新连接状态并通知订阅者（界面右上角的在线/离线指示灯） */
    private setConnected(v: boolean): void {
        if (this.connected !== v) {
            this.connected = v;
            this.statusCbs.forEach((cb) => cb(v));
        }
    }

    /** 把所有在途请求统一按失败唤醒，避免界面永久等待 */
    private failAllPending(reason: string): void {
        this.pending.forEach((resolve) => resolve({ ok: false, error: reason }));
        this.pending.clear();
    }

    isConnected(): boolean {
        return this.connected;
    }

    /** 发命令：分配 id → 登记到等待表 → 发送 → 等 onmessage 按 id 唤醒 */
    dispatch(payload: Record<string, unknown>): Promise<CmdResp> {
        if (!this.connected || !this.ws) {
            return Promise.resolve({ ok: false, error: "远程后端未连接" });
        }
        const id = this.nextId++;
        return new Promise((resolve) => {
            this.pending.set(id, resolve);
            this.ws!.send(JSON.stringify({ id, payload }));
            // 超时保护：10 秒没回复就按失败处理，避免 Promise 永久挂起
            setTimeout(() => {
                if (this.pending.has(id)) {
                    this.pending.delete(id);
                    resolve({ ok: false, error: "请求超时" });
                }
            }, 10000);
        });
    }

    onEvent(cb: (ev: Record<string, unknown>) => void): void {
        this.eventCbs.push(cb);
    }

    onStatusChange(cb: (connected: boolean) => void): void {
        this.statusCbs.push(cb);
    }

    /** 断开并停止一切活动（切换后端时调用）：
     *  先唤醒所有在途请求，再清掉回调、关闭连接；
     *  onclose 里不会再触发重连（disposed 已置位） */
    dispose(): void {
        this.disposed = true;
        this.failAllPending("后端已切换");
        this.eventCbs = [];
        this.statusCbs = [];
        this.ws?.close();
        this.ws = null;
    }
}

/** 从应用配置里挑出当前选中的后端项；
 *  旧格式（只有 backend/remoteUrl 没有列表）时把“远程”合成一个条目 */
function pickBackend(cfg: AppCfg): BackendEntry | null {
    const list = cfg.backends ?? [];
    const e = list.find((b) => b.id === cfg.backend) ?? null;
    if (!e && cfg.backend === "remote" && cfg.remoteUrl) {
        return { id: "remote", name: "远程后端", kind: "remote", url: cfg.remoteUrl };
    }
    return e;
}

/** 根据运行环境和后端条目创建合适的接入实例（启动/切换后端时调用） */
export async function createBackend(entry?: BackendEntry): Promise<Backend> {
    if (inTauri()) {
        // 桌面版：传入的条目优先（界面切换）；未传则读配置文件挑当前选中的项
        let e: BackendEntry | null = entry ?? null;
        if (!e) {
            const { invoke } = await import("@tauri-apps/api/core");
            try {
                e = pickBackend((await invoke("get_app_config")) as AppCfg);
            } catch {
                // 读取失败按本地模式处理
            }
        }
        if (e && e.kind === "remote" && e.url) {
            return new RemoteBackend(e.url);
        }
        return new LocalBackend();
    }
    // 纯浏览器模式：界面设置的列表（localStorage）优先，其次 ?ws= 参数，最后默认本机
    let e: BackendEntry | null = entry ?? null;
    if (!e) {
        try {
            const list = JSON.parse(
                localStorage.getItem("simx.backends") ?? "[]",
            ) as BackendEntry[];
            const id = localStorage.getItem("simx.backend") ?? "";
            e = list.find((b) => b.id === id) ?? null;
        } catch {
            // 配置损坏按默认处理
        }
    }
    if (e && e.kind === "remote" && e.url) {
        return new RemoteBackend(e.url);
    }
    const params = new URLSearchParams(location.search);
    const url = params.get("ws") ?? "ws://127.0.0.1:9800/ws";
    return new RemoteBackend(url);
}
