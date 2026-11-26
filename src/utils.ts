import { message } from "ant-design-vue";

/** 简单防抖：连续触发时只在最后一次静默 ms 后执行（用于 message:new 事件驱动的刷新） */
export function debounce<A extends unknown[]>(fn: (...args: A) => void, ms: number) {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return (...args: A) => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
}

export function formatNumber(n: number | undefined | null): string {
  return (n ?? 0).toLocaleString();
}

/** 按 1024 进制格式化文件字节数 */
export function formatBytes(n: number | undefined | null): string {
  if (n == null || !Number.isFinite(n) || n < 0) return "—";
  if (n < 1024) return `${n} B`;

  const units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
  let value = n / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${Number(value.toFixed(2))} ${units[unit]}`;
}

/** 毫秒时长的可读描述（秒 / 分秒 / 小时分） */
export function formatDuration(ms: number | undefined | null): string {
  const s = Math.floor((ms ?? 0) / 1000);
  if (s < 60) return `${s} 秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} 分 ${String(s % 60).padStart(2, "0")} 秒`;
  return `${Math.floor(m / 60)} 小时 ${m % 60} 分`;
}

export async function copyText(text: string, tip = "已复制") {
  try {
    await navigator.clipboard.writeText(text);
    message.success(tip);
  } catch {
    // 退化方案：选区复制
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    document.body.removeChild(ta);
    message.success(tip);
  }
}

/** 尝试美化 JSON，失败则原样返回 */
export function prettyJson(s: string): string {
  if (!s) return "";
  const t = s.trim();
  if (!t.startsWith("{") && !t.startsWith("[")) return s;
  try {
    return JSON.stringify(JSON.parse(t), null, 2);
  } catch {
    return s;
  }
}

/** SSE 流文本去掉 data: 前缀后尝试提取展示 */
export function prettySse(s: string): string {
  return s;
}
