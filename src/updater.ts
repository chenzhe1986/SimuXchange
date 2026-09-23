// 检查更新：从配置的"更新服务器"拉取 version.json，判断是否有新版本。
//
// 服务器（crates/simx-update-server）只需发布一个目录 + 一个版本号文件
// （release.txt 或 release.json，见服务器注释），version.json 由服务器
// 自动扫描目录生成，内容为：
//   {
//     "version": "1.3.0",            ← 发布版本号（用户在版本文件里写）
//     "files": [                       ← 发布目录里的全部文件（服务器自动扫描，
//       { "name": "simuxchange.exe",     含子目录；每项带大小与 sha256 校验值）
//         "size": 123, "sha256": ".." },
//       { "name": "resources/data.bin", ... }
//     ]
//   }
// 客户端把 files 里的文件全部下载到本地临时区（主 exe 最后），退出后原地覆盖。

/** 清单里的单个文件条目 */
export interface UpdateFileInfo {
    /** 相对发布根路径（可含子目录，"/" 分隔） */
    name: string;
    /** 文件大小（可省略） */
    size?: number;
    /** sha256 十六进制（可省略） */
    sha256?: string;
}

/** 服务器上的版本清单文件结构 */
export interface UpdateManifest {
    /** 服务器上的最新版本号，如 "1.3.0" */
    version: string;
    /** 发布目录里的文件清单（下载对象） */
    files: UpdateFileInfo[];
}

/** 检查结果：有新版时携带服务器信息，没有/检查失败为 null */
export interface UpdateInfo {
    manifest: UpdateManifest;
}

/**
 * 比较两个版本号，返回 a 相对 b 的大小：1 表示 a 更新，-1 表示更旧，0 相同。
 * 支持 "1.0.0" / "1.1" 等"数字点分"格式；无法解析时按 0 处理。
 */
export function compareVersions(a: string, b: string): number {
    const pa = a.split(".").map((n) => parseInt(n, 10) || 0);
    const pb = b.split(".").map((n) => parseInt(n, 10) || 0);
    const len = Math.max(pa.length, pb.length);
    for (let i = 0; i < len; i++) {
        const x = pa[i] ?? 0;
        const y = pb[i] ?? 0;
        if (x !== y) return x > y ? 1 : -1;
    }
    return 0;
}

/** 拼接下载地址：保证 updateUrl 与文件名之间恰好一个斜杠 */
export function joinUrl(base: string, name: string): string {
    return base.replace(/\/+$/, "") + "/" + name.replace(/^\/+/, "");
}

/**
 * 向更新服务器检查是否有新版本（浏览器环境直接 fetch 的备用路径，
 * 桌面端由 Rust 壳的 check_update_server 完成，见 App.vue）。
 * @param updateUrl 更新服务器根地址（如 http://192.168.1.10:8080，留空直接返回 null）
 * @param currentVersion 当前版本号（传入以方便测试与复用）
 * @returns 有新版时返回更新信息；已是最新、服务器不可达或清单无效时返回 null
 */
export async function checkForUpdate(
    updateUrl: string,
    currentVersion: string,
): Promise<UpdateInfo | null> {
    const base = updateUrl.trim();
    if (!base) return null;
    try {
        const resp = await fetch(joinUrl(base, "version.json"), { cache: "no-store" });
        if (!resp.ok) return null;
        const manifest = (await resp.json()) as UpdateManifest;
        if (!manifest.version) return null;
        // 版本号没比当前大就不算"有新版"
        if (compareVersions(manifest.version, currentVersion) <= 0) return null;
        return { manifest };
    } catch {
        // 服务器不可达（内网没开、地址填错等）静默失败：启动流程不能被更新检查拖累
        return null;
    }
}