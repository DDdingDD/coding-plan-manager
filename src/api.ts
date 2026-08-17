import { invoke } from "@tauri-apps/api/core";
import type {
  AggregatorView,
  CodingPlan,
  MessagePage,
  MessageRow,
  PlanView,
  StatsBucket,
  UsageStats,
} from "./types";

// 注意：tauri v2 命令参数在 JS 侧使用 camelCase 键名（Rust 侧 snake_case 自动映射）

// ---------------------------------------------------------------------------
// Coding Plans
// ---------------------------------------------------------------------------

export const listPlans = () => invoke<PlanView[]>("list_plans");

export const createPlan = (p: {
  name: string;
  baseUrl: string;
  authToken: string;
  remark?: string;
}) =>
  invoke<CodingPlan>("create_plan", {
    name: p.name,
    baseUrl: p.baseUrl,
    authToken: p.authToken,
    remark: p.remark,
  });

export const updatePlan = (p: {
  id: number;
  name: string;
  baseUrl: string;
  authToken: string;
  remark?: string;
  enabled?: boolean;
}) =>
  invoke<CodingPlan>("update_plan", {
    id: p.id,
    name: p.name,
    baseUrl: p.baseUrl,
    authToken: p.authToken,
    remark: p.remark,
    enabled: p.enabled,
  });

export const deletePlan = (id: number) => invoke<void>("delete_plan", { id });

// ---------------------------------------------------------------------------
// Aggregators
// ---------------------------------------------------------------------------

export const listAggregators = () => invoke<AggregatorView[]>("list_aggregators");

export const createAggregator = (p: {
  name: string;
  port?: number | null;
  tokenThreshold?: number | null;
}) =>
  invoke<AggregatorView>("create_aggregator", {
    name: p.name,
    port: p.port,
    tokenThreshold: p.tokenThreshold,
  });

export const updateAggregator = (p: {
  id: number;
  name: string;
  port: number;
  tokenThreshold: number;
}) =>
  invoke<AggregatorView>("update_aggregator", {
    id: p.id,
    name: p.name,
    port: p.port,
    tokenThreshold: p.tokenThreshold,
  });

export const deleteAggregator = (id: number) => invoke<void>("delete_aggregator", { id });

export const setAggregatorPlans = (aggregatorId: number, planIds: number[]) =>
  invoke<AggregatorView>("set_aggregator_plans", {
    aggregatorId,
    planIds,
  });

export const resetAggregatorUsage = (aggregatorId: number) =>
  invoke<AggregatorView>("reset_aggregator_usage", { aggregatorId });

export const startAggregator = (id: number) =>
  invoke<AggregatorView>("start_aggregator", { id });

export const stopAggregator = (id: number) =>
  invoke<AggregatorView>("stop_aggregator", { id });

// ---------------------------------------------------------------------------
// Messages & stats
// ---------------------------------------------------------------------------

export const listMessages = (p: {
  aggregatorId?: number | null;
  page?: number;
  pageSize?: number;
}) =>
  invoke<MessagePage>("list_messages", {
    aggregatorId: p.aggregatorId ?? null,
    page: p.page ?? 1,
    pageSize: p.pageSize ?? 20,
  });

export const getMessage = (id: number) => invoke<MessageRow>("get_message", { id });

export const clearMessages = (aggregatorId?: number | null) =>
  invoke<number>("clear_messages", { aggregatorId: aggregatorId ?? null });

export const globalStats = () => invoke<UsageStats>("global_stats");

/** 按天统计（近 days 天） */
export const dailyStats = (p: { aggregatorId?: number | null; days?: number } = {}) =>
  invoke<StatsBucket[]>("daily_stats", {
    aggregatorId: p.aggregatorId ?? null,
    days: p.days ?? 30,
  });

/** 按小时统计（date 为 YYYY-MM-DD，缺省今天） */
export const hourlyStats = (p: { aggregatorId?: number | null; date?: string } = {}) =>
  invoke<StatsBucket[]>("hourly_stats", {
    aggregatorId: p.aggregatorId ?? null,
    date: p.date ?? null,
  });

/** 按模型统计 */
export const modelStats = (p: { aggregatorId?: number | null } = {}) =>
  invoke<StatsBucket[]>("model_stats", { aggregatorId: p.aggregatorId ?? null });

// ---------------------------------------------------------------------------
// 事件订阅
// ---------------------------------------------------------------------------

import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** 订阅新消息推送（代理每次转发完成都会推送） */
export function onNewMessage(cb: (msg: MessageRow) => void): Promise<UnlistenFn> {
  return listen<MessageRow>("message:new", (e) => cb(e.payload));
}
