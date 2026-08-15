// 检查更新：从配置的“更新服务器”拉取 version.json，判断是否有新版本。
//
// 服务器上的文件布局（把发布文件放到任意 HTTP 可访问的目录即可）：
//   <updateUrl>/version.json        版本清单（下面这个结构）
//   <updateUrl>/<winInstaller>      Windows 安装包（如 SimuXchange_1.1.0_x64-setup.exe）
//   <updateUrl>/<linuxServer>       Linux 后端二进制（可选）
//
// version.json 内容示例：
//   {
//     "version": "1.1.0",
//     "notes": ["新增 XX 功能", "修复 XX 问题"],
//     "winInstaller": "SimuXchange_1.1.0_x64-setup.exe",
//     "linuxServer": "simx-server"
//   }

/** 服务器上的版本清单文件结构（字段缺失时按“无对应下载”处理） */
export interface UpdateManifest {
    /** 服务器上的最新版本号，如 "1.1.0" */
    version: string;
    /** 新版更新说明（一行一条） */
    notes?: string[];
    /** Windows 安装包文件名（相对于更新服务器根目录） */
    winInstaller?: string;
    /** Linux 后端二进制文件名（相对路径，可选） */
    linuxServer?: string;
}

/** 检查结果：有新版时携带服务器信息，没有或检查失败为 null */
export interface UpdateInfo {
    manifest: UpdateManifest;
    /** 安装包完整下载地址（winInstaller 缺失时为空串） */
    installerUrl: string;
}

/**
 * 比较两个版本号，返回 a 相对 b 的大小：1 表示 a 更新，-1 表示更旧，0 相同。
 * 支持 "1.0.0" / "1.1" 等“数字点分”格式；无法解析时按 0 处理。
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
 * 向更新服务器检查是否有新版本。
 * @param updateUrl 更新服务器根地址（如 http://192.168.1.10/update，留空直接返回 null）
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
        // 版本号没比当前大就不算“有新版”
        if (compareVersions(manifest.version, currentVersion) <= 0) return null;
        return {
            manifest,
            installerUrl: manifest.winInstaller ? joinUrl(base, manifest.winInstaller) : "",
        };
    } catch {
        // 服务器不可达（内网没开、地址填错等）静默失败：启动流程不能被更新检查拖累
        return null;
    }
}
