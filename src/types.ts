// 与 Rust 侧 serde 序列化结构一一对应（snake_case）

export interface UsageStats {
  total_tokens: number;
  prompt_tokens: number;
  completion_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  requests: number;
}

/** 分组统计的一个桶（key 为日期、小时或模型名） */
export interface StatsBucket {
  key: string;
  total_tokens: number;
  prompt_tokens: number;
  completion_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  requests: number;
}

/** 小计里程：自上次手动重置以来的用量（started_at 为 null 表示从未重置，计入全部历史；paused 时统计冻结） */
export interface TripStats {
  started_at: string | null;
  paused: boolean;
  stats: UsageStats;
}

export interface CodingPlan {
  id: number;
  name: string;
  base_url: string;
  auth_token: string;
  remark: string;
  /** 支持的模型名列表（空 = 未配置，模型匹配策略下不参与匹配） */
  models: string[];
  enabled: boolean;
  created_at: string;
}

/** list_plans 返回：计划字段 + 统计（serde flatten） */
export interface PlanView extends CodingPlan {
  stats: UsageStats;
}

export interface Aggregator {
  id: number;
  name: string;
  port: number;
  auth_token: string;
  strategy: string;
  token_threshold: number;
  created_at: string;
}

export interface BindingView {
  plan_id: number;
  plan_name: string;
  enabled: boolean;
  position: number;
  used_tokens: number;
  token_threshold: number;
  /** 该计划配置的支持模型（模型匹配策略的路由依据） */
  models: string[];
}

/** list_aggregators 返回：聚合器字段 + 运行状态 + 绑定 + 统计 */
export interface AggregatorView extends Aggregator {
  running: boolean;
  base_url: string;
  bindings: BindingView[];
  stats: UsageStats;
  /** 当前转发的计划（下一个请求将使用）；无可用计划时为 null */
  current_plan_id: number | null;
  /** 手动指定的当前计划（DB 原值）：阈值轮转的固定计划（null=自动轮转）、模型匹配的粘性目标 */
  manual_current_plan_id: number | null;
}

export interface MessageRow {
  id: number;
  aggregator_id: number;
  plan_id: number | null;
  method: string;
  path: string;
  status: number;
  request_body: string;
  response_body: string;
  model: string;
  prompt_tokens: number;
  completion_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  total_tokens: number;
  duration_ms: number;
  created_at: string;
  aggregator_name: string | null;
  plan_name: string | null;
}

/** 消息列表行（不含请求/响应体两个大字段；详情用 getMessage 拉取完整 MessageRow） */
export interface MessageSummary {
  id: number;
  aggregator_id: number;
  plan_id: number | null;
  method: string;
  path: string;
  status: number;
  model: string;
  prompt_tokens: number;
  completion_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  total_tokens: number;
  duration_ms: number;
  created_at: string;
  aggregator_name: string | null;
  plan_name: string | null;
}

export interface MessagePage {
  total: number;
  page: number;
  page_size: number;
  items: MessageSummary[];
}
