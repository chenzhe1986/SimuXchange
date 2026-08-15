// 拒单原因代码 → 说明文本的映射（数据源：docs/ 下的交易所错误码表）。
//
// 深交所网关用 docs/tgw_error.csv，上交所（竞价/新债券）用 docs/tdgw_error.csv。
// 两份表都是“错误码,说明文本”的 CSV（GBK 编码），已转为 UTF-8 放在
// src/assets/ 下随前端一起打包（?raw 导入），运行时不依赖 docs 目录。
// 说明文本里可能含逗号（如“证券停牌-证券停牌,不允许该类申报委托”），
// 所以解析时只按第一个逗号切开，其余部分整段作为文本。
import type { GatewayCategory } from "./types";
import tgwErrors from "./assets/tgw_error.csv?raw";
import tdgwErrors from "./assets/tdgw_error.csv?raw";

/** 把“代码,说明”的 CSV 文本解析成 代码 → 说明 的映射（说明含逗号也能正确切分） */
function parseErrorCsv(raw: string): Map<number, string> {
    const map = new Map<number, string>();
    for (const line of raw.split(/\r?\n/)) {
        const idx = line.indexOf(",");
        if (idx <= 0) continue; // 空行 / 没有逗号的行直接跳过
        const code = Number(line.slice(0, idx));
        if (Number.isNaN(code)) continue;
        map.set(code, line.slice(idx + 1));
    }
    return map;
}

/** 深圳 TGW 错误码表（拒单说明文本来源） */
const TGW_ERRORS = parseErrorCsv(tgwErrors);
/** 上海 TDGW 错误码表（拒单说明文本来源） */
const TDGW_ERRORS = parseErrorCsv(tdgwErrors);

/** 按网关分类取错误码表：深交所用 TGW 表，上交所（竞价/新债券）用 TDGW 表 */
function errorMapOf(category: GatewayCategory): Map<number, string> {
    return category === "sz" ? TGW_ERRORS : TDGW_ERRORS;
}

/** 按错误码表查拒单说明文本；表中没有该代码时返回 undefined（界面保留手动输入） */
export function rejectTextOf(category: GatewayCategory, reason: number): string | undefined {
    return errorMapOf(category).get(reason);
}
