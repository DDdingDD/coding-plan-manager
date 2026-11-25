<template>
  <section class="message-sequence" aria-labelledby="message-sequence-title">
    <div class="sequence-header">
      <span id="message-sequence-title" class="sequence-title">请求时序</span>
      <span class="sequence-hint">点击步骤查看对应报文</span>
    </div>

    <div class="actors">
      <div
        v-for="actor in actors"
        :key="actor.label"
        class="actor"
        :class="{ muted: actor.muted }"
      >
        <span class="actor-icon" aria-hidden="true">
          <component :is="actor.icon" />
        </span>
        <span class="actor-label">{{ actor.label }}</span>
        <span class="actor-name">{{ actor.name }}</span>
      </div>
    </div>

    <div class="sequence-body">
      <div class="lifelines" aria-hidden="true">
        <span v-for="actor in actors" :key="actor.label"></span>
      </div>
      <ol class="sequence-steps">
        <li v-for="(step, index) in steps" :key="step.id">
          <component
            :is="step.body ? 'button' : 'div'"
            :type="step.body ? 'button' : undefined"
            class="sequence-step"
            :class="{ interactive: step.body }"
            :aria-label="step.body ? `${step.title}，查看${step.body === 'req' ? '请求体' : '响应体'}` : undefined"
            @click="openStep(step)"
          >
            <span class="step-number" aria-hidden="true">{{ index + 1 }}</span>
            <span class="step-title">
              <component
                :is="iconFor(step.tone)"
                v-if="step.tone"
                class="status-icon"
                :class="`is-${step.tone}`"
                aria-hidden="true"
              />
              <span>{{ step.title }}</span>
              <span v-if="step.status !== undefined" class="http-status">HTTP {{ step.status }}</span>
              <span v-if="step.format" class="response-format">{{ step.format }}</span>
            </span>
            <span class="step-detail">{{ step.detail }}</span>
            <span v-if="step.direction !== 'none'" class="step-flow" aria-hidden="true">
              <span
                class="flow-line"
                :class="[step.direction, { response: step.response }]"
              ></span>
            </span>
          </component>
        </li>
      </ol>
    </div>

    <dl class="token-summary">
      <div class="summary-label"><dt>Token 用量</dt></div>
      <div>
        <dt>总计</dt>
        <dd>{{ formatNumber(message.total_tokens) }}</dd>
      </div>
      <div>
        <dt>输入</dt>
        <dd>{{ formatNumber(message.prompt_tokens) }}</dd>
      </div>
      <div>
        <dt>输出</dt>
        <dd>{{ formatNumber(message.completion_tokens) }}</dd>
      </div>
      <template v-if="message.cache_read_tokens > 0 || message.cache_creation_tokens > 0">
        <div>
          <dt>缓存读</dt>
          <dd>{{ formatNumber(message.cache_read_tokens) }}</dd>
        </div>
        <div>
          <dt>缓存写</dt>
          <dd>{{ formatNumber(message.cache_creation_tokens) }}</dd>
        </div>
      </template>
    </dl>
  </section>
</template>

<script setup lang="ts">
import { computed } from "vue";
import {
  ApiOutlined,
  CheckCircleFilled,
  CloseCircleFilled,
  CloudServerOutlined,
  ExclamationCircleFilled,
  LaptopOutlined,
} from "@ant-design/icons-vue";
import type { MessageRow } from "../types";
import { formatNumber } from "../utils";

type BodyTab = "req" | "resp";
type StepTone = "success" | "warning" | "error";
type Direction =
  | "client-aggregator"
  | "aggregator-client"
  | "aggregator-plan"
  | "plan-aggregator"
  | "none";

interface SequenceStep {
  id: string;
  title: string;
  detail: string;
  direction: Direction;
  body?: BodyTab;
  response?: boolean;
  status?: number;
  format?: "JSON" | "SSE";
  tone?: StepTone;
}

const props = defineProps<{ message: MessageRow }>();
const emit = defineEmits<{ "open-body": [kind: BodyTab] }>();

const statusIcons = {
  success: CheckCircleFilled,
  warning: ExclamationCircleFilled,
  error: CloseCircleFilled,
};

const actors = computed(() => [
  { label: "客户端", name: "API 调用方", icon: LaptopOutlined },
  {
    label: "聚合器",
    name: props.message.aggregator_name ?? `聚合器 #${props.message.aggregator_id}`,
    icon: CloudServerOutlined,
  },
  {
    label: "上游计划",
    name:
      props.message.plan_id === null
        ? "未选择计划"
        : (props.message.plan_name ?? `计划 #${props.message.plan_id}`),
    icon: ApiOutlined,
    muted: props.message.plan_id === null,
  },
]);

/** 仅根据保存的响应副本识别格式，不用请求中的 stream 猜测上游响应类型。 */
function detectResponseFormat(body: string): "JSON" | "SSE" | undefined {
  const text = body.trim();
  if (!text) return undefined;
  try {
    JSON.parse(text);
    return "JSON";
  } catch {
    // 非 JSON 时再检查 SSE；截断的 JSON 不应误标为完整 JSON。
  }

  const lines = text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && line !== "…[已截断]");
  const isSse =
    lines.some((line) => line.startsWith("data:")) &&
    lines.every((line) => /^(?:data|event|id|retry):/.test(line) || line.startsWith(":"));
  return isSse ? "SSE" : undefined;
}

function toneFor(status: number): StepTone | undefined {
  if (status >= 200 && status < 300) return "success";
  if (status === 401 || status === 403) return "warning";
  if (status >= 400) return "error";
  return undefined;
}

const steps = computed<SequenceStep[]>(() => {
  const message = props.message;
  const format = detectResponseFormat(message.response_body);
  const tone = toneFor(message.status);
  const result: SequenceStep[] = [
    {
      id: "receive",
      title: "接收请求",
      detail: `${message.method} ${message.path}${message.model ? ` · ${message.model}` : ""}`,
      direction: "client-aggregator",
      body: "req",
    },
  ];

  if (message.plan_id === null) {
    const title =
      message.status === 401
        ? "鉴权失败，未转发上游"
        : message.status === 503
          ? "无可用 Coding Plan，未转发上游"
          : "请求在聚合器终止，未转发上游";
    const detail =
      message.status === 401
        ? "请检查客户端 AUTH_TOKEN"
        : message.status === 503
          ? "未绑定可用计划，或绑定计划已全部禁用"
          : "详情见响应记录";
    result.push(
      { id: "stop", title, detail, direction: "none", tone },
      {
        id: "return",
        title: message.status === 401 ? "返回鉴权失败" : "返回本地错误响应",
        detail: `总耗时 ${message.duration_ms} ms`,
        direction: "aggregator-client",
        body: "resp",
        response: true,
        status: message.status,
        format,
        tone,
      },
    );
    return result;
  }

  result.push({
    id: "forward",
    title: "转发至上游",
    detail: "替换鉴权令牌并透传业务请求头",
    direction: "aggregator-plan",
    body: "req",
  });

  const upstreamTitle =
    message.status === 502
      ? "上游阶段异常"
      : message.status >= 400
        ? "上游返回错误"
        : "上游响应";
  result.push(
    {
      id: "upstream-response",
      title: upstreamTitle,
      detail:
        message.status === 502
          ? "已选择计划并尝试转发 · 详情见响应记录"
          : format === "SSE"
            ? "接收并记录上游事件流"
            : "接收上游返回内容",
      direction: "plan-aggregator",
      body: "resp",
      response: true,
      status: message.status,
      format,
      tone,
    },
    {
      id: "return",
      title: message.status >= 400 ? "返回错误响应" : "返回响应",
      detail: `${format === "SSE" ? "透传 SSE 事件流 · " : ""}总耗时 ${message.duration_ms} ms`,
      direction: "aggregator-client",
      body: "resp",
      response: true,
      status: message.status,
      format,
      tone,
    },
  );
  return result;
});

function iconFor(tone?: StepTone) {
  return tone ? statusIcons[tone] : null;
}

function openStep(step: SequenceStep) {
  if (step.body) emit("open-body", step.body);
}
</script>

<style scoped>
.message-sequence {
  margin-bottom: 16px;
  overflow: hidden;
  background: #fff;
  border: 1px solid #f0f0f0;
  border-radius: 8px;
}

.sequence-header {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 12px;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  background: #fafafa;
  border-bottom: 1px solid #f0f0f0;
}

.sequence-title {
  font-size: 14px;
  font-weight: 600;
  color: #262626;
}

.sequence-hint {
  font-size: 12px;
  color: #8c8c8c;
}

.actors {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  padding: 16px 12px 12px;
}

.actor {
  display: flex;
  flex-direction: column;
  gap: 3px;
  align-items: center;
  min-width: 0;
  text-align: center;
}

.actor-icon {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 32px;
  height: 32px;
  margin-bottom: 3px;
  font-size: 17px;
  color: #1677ff;
  background: #e6f4ff;
  border-radius: 50%;
}

.actor.muted .actor-icon {
  color: #8c8c8c;
  background: #f5f5f5;
}

.actor-label {
  font-size: 12px;
  color: #8c8c8c;
}

.actor-name {
  max-width: 100%;
  overflow-wrap: anywhere;
  font-size: 12px;
  font-weight: 600;
  line-height: 18px;
  color: #595959;
}

.actor.muted .actor-name {
  font-weight: 400;
  color: #8c8c8c;
}

.sequence-body {
  position: relative;
  padding-bottom: 6px;
}

.lifelines {
  position: absolute;
  inset: 0 12px;
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  pointer-events: none;
}

.lifelines span {
  width: 1px;
  height: 100%;
  margin: 0 auto;
  border-left: 1px dashed #e8e8e8;
}

.sequence-steps {
  position: relative;
  padding: 0 12px;
  margin: 0;
  list-style: none;
}

.sequence-step {
  position: relative;
  display: flex;
  flex-direction: column;
  width: 100%;
  min-height: 64px;
  padding: 8px 0 2px;
  margin: 0;
  font: inherit;
  color: inherit;
  text-align: center;
  background: transparent;
  border: 0;
  border-radius: 6px;
}

.sequence-step.interactive {
  cursor: pointer;
}

.sequence-step.interactive:hover {
  background: rgb(22 119 255 / 5%);
}

.sequence-step.interactive:focus-visible {
  outline: 2px solid #1677ff;
  outline-offset: -2px;
}

.step-number {
  position: absolute;
  top: 10px;
  left: 8px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 18px;
  height: 18px;
  font-size: 11px;
  line-height: 1;
  color: #8c8c8c;
  background: #fff;
  border: 1px solid #d9d9d9;
  border-radius: 50%;
}

.step-title {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 6px;
  align-items: center;
  justify-content: center;
  min-width: 0;
  padding: 0 30px;
  font-size: 13px;
  font-weight: 600;
  line-height: 20px;
  overflow-wrap: anywhere;
  color: #262626;
}

.status-icon {
  font-size: 14px;
}

.status-icon.is-success {
  color: #52c41a;
}

.status-icon.is-warning {
  color: #faad14;
}

.status-icon.is-error {
  color: #ff4d4f;
}

.http-status,
.response-format {
  padding: 0 5px;
  font-family: monospace;
  font-size: 11px;
  font-weight: 400;
  line-height: 17px;
  color: #595959;
  background: #f5f5f5;
  border: 1px solid #e8e8e8;
  border-radius: 4px;
}

.response-format {
  color: #1677ff;
  background: #e6f4ff;
  border-color: #bae0ff;
}

.step-detail {
  min-width: 0;
  margin: 2px 30px 0;
  overflow-wrap: anywhere;
  font-size: 12px;
  line-height: 18px;
  color: #8c8c8c;
}

.step-flow {
  position: relative;
  display: block;
  flex: 1;
  min-height: 22px;
  margin-top: 2px;
}

.flow-line {
  position: absolute;
  top: 10px;
  width: 33.3333%;
  height: 0;
  border-top: 2px solid #1677ff;
}

.flow-line.client-aggregator,
.flow-line.aggregator-client {
  left: 16.6667%;
}

.flow-line.aggregator-plan,
.flow-line.plan-aggregator {
  left: 50%;
}

.flow-line.response {
  border-top-style: dashed;
  border-top-color: #8c8c8c;
}

.flow-line.client-aggregator::after,
.flow-line.aggregator-plan::after,
.flow-line.aggregator-client::before,
.flow-line.plan-aggregator::before {
  position: absolute;
  top: -5px;
  width: 0;
  height: 0;
  content: "";
  border-top: 4px solid transparent;
  border-bottom: 4px solid transparent;
}

.flow-line.client-aggregator::after,
.flow-line.aggregator-plan::after {
  right: -1px;
  border-left: 7px solid #1677ff;
}

.flow-line.aggregator-client::before,
.flow-line.plan-aggregator::before {
  left: -1px;
  border-right: 7px solid #8c8c8c;
}

.token-summary {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  align-items: center;
  padding: 10px 16px;
  margin: 0;
  background: #fafafa;
  border-top: 1px solid #f0f0f0;
}

.summary-label {
  margin-right: auto;
  font-size: 12px;
  font-weight: 600;
  color: #595959;
}

.token-summary div {
  display: flex;
  gap: 5px;
  align-items: baseline;
}

.token-summary dt {
  font-size: 12px;
  color: #8c8c8c;
}

.token-summary dd {
  margin: 0;
  font-size: 12px;
  font-variant-numeric: tabular-nums;
  font-weight: 600;
  color: #262626;
}

@media (forced-colors: active) {
  .message-sequence,
  .sequence-step.interactive:focus-visible,
  .http-status,
  .response-format,
  .step-number {
    border-color: CanvasText;
  }

  .flow-line {
    border-top-color: CanvasText;
  }

  .flow-line.client-aggregator::after,
  .flow-line.aggregator-plan::after {
    border-left-color: CanvasText;
  }

  .flow-line.aggregator-client::before,
  .flow-line.plan-aggregator::before {
    border-right-color: CanvasText;
  }
}
</style>
