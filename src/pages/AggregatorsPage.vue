<template>
  <div>
    <a-flex justify="space-between" align="center" style="margin-bottom: 12px">
      <a-typography-title :level="4" style="margin: 0">Plan Aggregator</a-typography-title>
      <a-button type="primary" @click="openCreate">新建聚合器</a-button>
    </a-flex>

    <a-table
      :columns="columns"
      :data-source="aggs"
      :loading="loading"
      row-key="id"
      :pagination="false"
      size="middle"
    >
      <template #bodyCell="{ column, record }">
        <template v-if="column.key === 'status'">
          <a-tag :color="record.running ? 'green' : 'default'">
            {{ record.running ? "运行中" : "已停止" }}
          </a-tag>
        </template>
        <template v-else-if="column.key === 'base_url'">
          <a-flex :gap="4" align="center">
            <a-typography-text code style="max-width: 200px" :ellipsis="true">
              {{ record.base_url }}
            </a-typography-text>
            <a-button size="small" type="text" @click="copyText(record.base_url, 'BASE_URL 已复制')">
              <CopyOutlined />
            </a-button>
          </a-flex>
        </template>
        <template v-else-if="column.key === 'auth_token'">
          <a-flex :gap="4" align="center">
            <a-typography-text code style="max-width: 170px" :ellipsis="true">
              {{ showToken[record.id] ? record.auth_token : maskToken(record.auth_token) }}
            </a-typography-text>
            <a-button size="small" type="text" @click="showToken[record.id] = !showToken[record.id]">
              <EyeOutlined v-if="!showToken[record.id]" />
              <EyeInvisibleOutlined v-else />
            </a-button>
            <a-button size="small" type="text" @click="copyText(record.auth_token, 'AUTH_TOKEN 已复制')">
              <CopyOutlined />
            </a-button>
          </a-flex>
        </template>
        <template v-else-if="column.key === 'plans'">
          <a-tooltip v-for="b in record.bindings" :key="b.plan_id" :title="bindingTitle(b, record)">
            <a-tag
              :color="b.plan_id === record.current_plan_id ? 'green' : 'default'"
              :style="{ margin: 2, opacity: b.enabled ? 1 : 0.45 }"
            >
              <AimOutlined v-if="b.plan_id === record.current_plan_id" style="margin-right: 4px" />
              {{ b.plan_name }}{{ b.enabled ? "" : "（禁）" }}
            </a-tag>
          </a-tooltip>
          <a-tag v-if="!record.bindings.length" color="warning">未绑定</a-tag>
          <a-tag v-else-if="record.current_plan_id == null" color="error">无可用计划</a-tag>
        </template>
        <template v-else-if="column.key === 'stats'">
          {{ formatNumber(record.stats.total_tokens) }} tokens / {{ formatNumber(record.stats.requests) }} 次
        </template>
        <template v-else-if="column.key === 'actions'">
          <a-flex :gap="4">
            <a-button
              size="small"
              :type="record.running ? 'default' : 'primary'"
              :loading="toggling[record.id]"
              @click="toggleRun(record as AggregatorView)"
            >
              {{ record.running ? "停止" : "启动" }}
            </a-button>
            <a-button size="small" @click="openEdit(record as AggregatorView)">编辑</a-button>
            <a-button size="small" @click="openDetail(record as AggregatorView)">详情</a-button>
            <a-popconfirm
              :title="record.running ? '运行中的服务将被停止并删除，确定？' : '确定删除？'"
              @confirm="removeAgg(record.id)"
            >
              <a-button size="small" danger>删除</a-button>
            </a-popconfirm>
          </a-flex>
        </template>
      </template>
    </a-table>

    <!-- 新建/编辑 -->
    <a-modal
      v-model:open="modalOpen"
      :title="editing ? '编辑聚合器' : '新建聚合器'"
      :confirm-loading="submitting"
      destroy-on-close
      @ok="submit"
    >
      <a-form layout="vertical" style="margin-top: 12px">
        <a-form-item label="名称" required>
          <a-input v-model:value="form.name" placeholder="如：主力聚合器" />
        </a-form-item>
        <a-form-item label="端口" extra="留空自动分配（8300-8399）；服务运行中不可修改">
          <a-input-number v-model:value="form.port" :min="1" :max="65535" style="width: 100%" placeholder="自动分配" />
        </a-form-item>
        <a-form-item label="路由策略">
          <a-radio-group v-model:value="form.strategy">
            <a-radio-button value="threshold_rotation">阈值轮转</a-radio-button>
            <a-radio-button value="model_match">模型匹配</a-radio-button>
          </a-radio-group>
        </a-form-item>
        <a-form-item
          v-if="form.strategy === 'threshold_rotation'"
          label="单计划 token 阈值"
          extra="按绑定顺序轮转；达到阈值切换下一个，全部超额清零回绕。详情中可手动固定当前计划"
        >
          <a-input-number
            v-model:value="form.token_threshold"
            :min="1"
            :step="100000"
            style="width: 100%"
          />
        </a-form-item>
        <a-typography-paragraph v-else type="secondary" style="margin-bottom: 0; font-size: 12px">
          按请求携带的模型名匹配各计划配置的「支持模型」路由；当前计划不在匹配集中时切到用量最少的匹配计划，
          无匹配则走当前计划。当前计划可在详情中手动切换。
        </a-typography-paragraph>
      </a-form>
    </a-modal>

    <!-- 详情抽屉 -->
    <a-drawer v-model:open="detailOpen" width="640" :title="detail?.name ?? ''">
      <template v-if="detail">
        <a-card size="small" title="接入参数" style="margin-bottom: 16px">
          <a-descriptions :column="1" size="small">
            <a-descriptions-item label="BASE_URL">
              <a-flex :gap="6" align="center">
                <a-typography-text code>{{ detail.base_url }}</a-typography-text>
                <a-button size="small" type="text" @click="copyText(detail.base_url, '已复制')">
                  <CopyOutlined />
                </a-button>
              </a-flex>
            </a-descriptions-item>
            <a-descriptions-item label="AUTH_TOKEN">
              <a-flex :gap="6" align="center">
                <a-typography-text code>{{ detail.auth_token }}</a-typography-text>
                <a-button size="small" type="text" @click="copyText(detail.auth_token, '已复制')">
                  <CopyOutlined />
                </a-button>
              </a-flex>
            </a-descriptions-item>
          </a-descriptions>
          <a-typography-paragraph type="secondary" style="margin-bottom: 0; font-size: 12px">
            Claude Code 接入：设置环境变量 ANTHROPIC_BASE_URL={{ detail.base_url }}、
            ANTHROPIC_AUTH_TOKEN=&lt;上方令牌&gt;。状态需为「运行中」。
          </a-typography-paragraph>
        </a-card>

        <a-card size="small" title="转发策略" style="margin-bottom: 16px">
          <a-flex align="center" :gap="8" style="margin-bottom: 8px">
            <span>当前转发：</span>
            <a-tag v-if="currentPlanName" color="green">
              <AimOutlined style="margin-right: 4px" />{{ currentPlanName }}
            </a-tag>
            <a-tag v-else color="error">无可用计划</a-tag>
            <a-typography-text type="secondary" style="font-size: 12px">
              {{ detail.strategy === "model_match" ? "无匹配模型时走此计划，匹配命中即自动切换" : "下一请求将转发到此计划" }}
            </a-typography-text>
          </a-flex>
          <a-flex align="center" :gap="8" style="margin-bottom: 8px">
            <span>当前计划：</span>
            <a-select
              v-if="detail.strategy === 'model_match'"
              :value="detail.current_plan_id"
              style="width: 220px"
              :options="currentPlanOptions"
              :loading="settingCurrent"
              @change="(v: any) => onSetCurrentPlan(v as number)"
            />
            <a-select
              v-else
              :value="detail.manual_current_plan_id ?? undefined"
              style="width: 220px"
              :options="currentPlanOptions"
              :loading="settingCurrent"
              allow-clear
              placeholder="自动轮转"
              @change="(v: any) => onSetCurrentPlan((v ?? null) as number | null)"
            />
            <a-typography-text type="secondary" style="font-size: 12px">
              {{
                detail.strategy === "model_match"
                  ? "手动指定下一请求的兜底计划；模型匹配命中时会自动切换"
                  : "固定当前转发的计划；该计划达阈值或失效后自动恢复轮转，清空则回到自动"
              }}
            </a-typography-text>
          </a-flex>
          <a-flex align="center" :gap="8">
            <a-radio-group
              :value="detail.strategy"
              size="small"
              button-style="solid"
              :disabled="switchingStrategy"
              @change="(e: any) => onStrategyChange(e.target.value as string)"
            >
              <a-radio-button value="threshold_rotation">阈值轮转</a-radio-button>
              <a-radio-button value="model_match">模型匹配</a-radio-button>
            </a-radio-group>
            <template v-if="detail.strategy === 'threshold_rotation'">
              <span>单计划阈值</span>
              <a-input-number
                v-model:value="thresholdDraft"
                :min="1"
                :step="100000"
                size="small"
                style="width: 140px"
              />
              <a-button size="small" @click="saveThreshold" :loading="savingThreshold">保存</a-button>
            </template>
            <a-button size="small" @click="resetUsage" :loading="resetting">重置用量</a-button>
          </a-flex>
        </a-card>

        <a-card
          size="small"
          :title="detail.strategy === 'model_match' ? '绑定的 Coding Plan（按模型匹配路由）' : '绑定的 Coding Plan（自上而下轮转）'"
        >
          <a-table
            :columns="bindingColumns"
            :data-source="detail.bindings"
            :pagination="false"
            size="small"
            row-key="plan_id"
          >
            <template #bodyCell="{ column, record, index }">
              <template v-if="column.key === 'plan_name'">
                {{ record.plan_name }}
                <a-tag
                  v-if="record.plan_id === detail?.current_plan_id"
                  color="green"
                  style="margin-left: 4px"
                >
                  当前
                </a-tag>
              </template>
              <template v-else-if="column.key === 'models'">
                <template v-if="record.models?.length">
                  <a-tag v-for="m in record.models" :key="m" style="margin: 2px">{{ m }}</a-tag>
                </template>
                <a-typography-text v-else type="secondary">-</a-typography-text>
              </template>
              <template v-else-if="column.key === 'usage'">
                <!-- 进度条与数字上下堆叠并定宽列，避免长数字横向溢出遮挡操作列 -->
                <a-flex vertical :gap="2" style="min-width: 0">
                  <a-progress
                    :percent="Math.min(100, Math.round((record.used_tokens / Math.max(1, record.token_threshold)) * 100))"
                    :status="record.used_tokens >= record.token_threshold ? 'exception' : 'active'"
                    size="small"
                    :show-info="false"
                    style="margin: 0"
                  />
                  <a-typography-text
                    type="secondary"
                    style="display: block; font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis"
                  >
                    {{ formatNumber(record.used_tokens) }} / {{ formatNumber(record.token_threshold) }}
                  </a-typography-text>
                </a-flex>
              </template>
              <template v-else-if="column.key === 'actions'">
                <a-flex :gap="2">
                  <a-button size="small" type="text" :disabled="index === 0" @click="moveBinding(index as number, -1)">
                    <UpOutlined />
                  </a-button>
                  <a-button
                    size="small"
                    type="text"
                    :disabled="index === (detail?.bindings.length ?? 0) - 1"
                    @click="moveBinding(index as number, 1)"
                  >
                    <DownOutlined />
                  </a-button>
                  <a-popconfirm title="解除绑定？" @confirm="removeBinding(index as number)">
                    <a-button size="small" type="text" danger>
                      <DeleteOutlined />
                    </a-button>
                  </a-popconfirm>
                </a-flex>
              </template>
            </template>
          </a-table>

          <a-flex :gap="8" style="margin-top: 12px">
            <a-select
              v-model:value="planToAdd"
              placeholder="选择要绑定的 Coding Plan"
              style="flex: 1"
              :options="bindableOptions"
              show-search
              option-filter-prop="label"
            />
            <a-button @click="addBinding" :disabled="!planToAdd">绑定</a-button>
          </a-flex>
        </a-card>
      </template>
    </a-drawer>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref } from "vue";
import { message } from "ant-design-vue";
import {
  AimOutlined,
  CopyOutlined,
  DeleteOutlined,
  DownOutlined,
  EyeInvisibleOutlined,
  EyeOutlined,
  UpOutlined,
} from "@ant-design/icons-vue";
import {
  createAggregator,
  deleteAggregator,
  listAggregators,
  listPlans,
  resetAggregatorUsage,
  setAggregatorCurrentPlan,
  setAggregatorPlans,
  startAggregator,
  stopAggregator,
  updateAggregator,
  onNewMessage,
} from "../api";
import type { AggregatorView, BindingView } from "../types";
import { copyText, debounce, formatNumber } from "../utils";

/** 路由策略标识（与 Rust 侧 db.rs 常量一致） */
const STRATEGY_THRESHOLD_ROTATION = "threshold_rotation";
const STRATEGY_MODEL_MATCH = "model_match";

const strategyLabel = (s: string) =>
  s === STRATEGY_MODEL_MATCH ? "模型匹配" : "阈值轮转";

const columns = [
  { title: "名称", dataIndex: "name", key: "name", width: 140 },
  { title: "状态", key: "status", width: 90 },
  { title: "BASE_URL", key: "base_url" },
  { title: "AUTH_TOKEN", key: "auth_token" },
  { title: "绑定计划", key: "plans" },
  { title: "累计消耗", key: "stats", width: 180 },
  { title: "操作", key: "actions", width: 260 },
];

/** 绑定表格列：仅模型匹配策略显示「支持模型」（阈值轮转不依赖该列） */
const bindingColumns = computed(() => {
  const cols: { title: string; dataIndex?: string; key: string; width?: number }[] = [
    { title: "顺序", dataIndex: "position", key: "position", width: 60 },
    { title: "计划", dataIndex: "plan_name", key: "plan_name" },
  ];
  if (detail.value?.strategy === STRATEGY_MODEL_MATCH) {
    cols.push({ title: "支持模型", key: "models", width: 160 });
  }
  cols.push(
    { title: "已用 / 阈值", key: "usage", width: 170 },
    { title: "操作", key: "actions", width: 120 },
  );
  return cols;
});

const aggs = ref<AggregatorView[]>([]);
const loading = ref(false);
const showToken = reactive<Record<number, boolean>>({});
const toggling = reactive<Record<number, boolean>>({});

const maskToken = (t: string) => (t.length > 8 ? `${t.slice(0, 5)}••••••${t.slice(-4)}` : "••••");

/** 绑定 tag 的悬停提示：当前转发（阈值轮转区分手动固定/自动轮转）/ 已禁用 / 模型匹配的支持模型 / 已达阈值 / 用量 */
function bindingTitle(b: BindingView, agg: AggregatorView): string {
  const usage = `已用 ${formatNumber(b.used_tokens)} / 阈值 ${formatNumber(b.token_threshold)}`;
  if (b.plan_id === agg.current_plan_id) {
    const pinned =
      agg.strategy === STRATEGY_THRESHOLD_ROTATION && b.plan_id === agg.manual_current_plan_id;
    return `${pinned ? "手动固定当前转发" : "当前转发到此计划"}（${usage}）`;
  }
  if (!b.enabled) return "已禁用，不参与轮转";
  if (agg.strategy === STRATEGY_MODEL_MATCH) {
    const models = b.models?.length ? `，支持 ${b.models.join("、")}` : "";
    return `${usage}${models}`;
  }
  if (b.used_tokens >= b.token_threshold) return `已达阈值，等待回绕（${usage}）`;
  return usage;
}

async function refresh() {
  loading.value = true;
  try {
    aggs.value = await listAggregators();
    if (detail.value) {
      const fresh = aggs.value.find((a) => a.id === detail.value!.id);
      if (fresh) detail.value = fresh;
    }
  } catch (e) {
    message.error(String(e));
  } finally {
    loading.value = false;
  }
}

// ---- 新建 / 编辑 ----
const modalOpen = ref(false);
const submitting = ref(false);
const editing = ref<AggregatorView | null>(null);
const form = reactive<{
  name: string;
  port: number | null;
  token_threshold: number;
  strategy: string;
}>({
  name: "",
  port: null,
  token_threshold: 1000000,
  strategy: STRATEGY_THRESHOLD_ROTATION,
});

function openCreate() {
  editing.value = null;
  Object.assign(form, {
    name: "",
    port: null,
    token_threshold: 1000000,
    strategy: STRATEGY_THRESHOLD_ROTATION,
  });
  modalOpen.value = true;
}

function openEdit(agg: AggregatorView) {
  editing.value = agg;
  Object.assign(form, {
    name: agg.name,
    port: agg.port,
    token_threshold: agg.token_threshold,
    strategy: agg.strategy,
  });
  modalOpen.value = true;
}

async function submit() {
  submitting.value = true;
  try {
    if (editing.value) {
      await updateAggregator({
        id: editing.value.id,
        name: form.name,
        port: form.port ?? editing.value.port,
        tokenThreshold: form.token_threshold,
        strategy: form.strategy,
      });
      message.success("已更新");
    } else {
      await createAggregator({
        name: form.name,
        port: form.port,
        tokenThreshold: form.token_threshold,
        strategy: form.strategy,
      });
      message.success("已创建，令牌已自动生成");
    }
    modalOpen.value = false;
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    submitting.value = false;
  }
}

async function removeAgg(id: number) {
  try {
    await deleteAggregator(id);
    message.success("已删除");
    if (detail.value?.id === id) detailOpen.value = false;
    await refresh();
  } catch (e) {
    message.error(String(e));
  }
}

// ---- 启停 ----
async function toggleRun(agg: AggregatorView) {
  toggling[agg.id] = true;
  try {
    if (agg.running) {
      await stopAggregator(agg.id);
      message.success("已停止");
    } else {
      await startAggregator(agg.id);
      message.success(`已启动：${agg.base_url}`);
    }
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    toggling[agg.id] = false;
  }
}

// ---- 详情抽屉 ----
const detailOpen = ref(false);
const detail = ref<AggregatorView | null>(null);
/** 当前转发计划（current_plan_id 对应绑定）的名称，无可用计划时为 null */
const currentPlanName = computed(() => {
  if (!detail.value) return null;
  return (
    detail.value.bindings.find((b) => b.plan_id === detail.value!.current_plan_id)?.plan_name ?? null
  );
});
const thresholdDraft = ref<number>(0);
const savingThreshold = ref(false);
const switchingStrategy = ref(false);
const resetting = ref(false);
const planToAdd = ref<number | null>(null);
const allPlans = ref<{ id: number; name: string; enabled: boolean }[]>([]);

const bindableOptions = computed(() => {
  if (!detail.value) return [];
  const bound = new Set(detail.value.bindings.map((b) => b.plan_id));
  return allPlans.value
    .filter((p) => !bound.has(p.id))
    .map((p) => ({ value: p.id, label: p.name + (p.enabled ? "" : "（已禁用）") }));
});

function openDetail(agg: AggregatorView) {
  detail.value = agg;
  thresholdDraft.value = agg.token_threshold;
  detailOpen.value = true;
  listPlans().then((ps) => (allPlans.value = ps.map((p) => ({ id: p.id, name: p.name, enabled: p.enabled }))));
}

async function saveThreshold() {
  if (!detail.value) return;
  savingThreshold.value = true;
  try {
    detail.value = await updateAggregator({
      id: detail.value.id,
      name: detail.value.name,
      port: detail.value.port,
      tokenThreshold: thresholdDraft.value,
      strategy: detail.value.strategy,
    });
    message.success("阈值已保存");
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    savingThreshold.value = false;
  }
}

/** 抽屉内直接切换路由策略（配置逐请求重读，运行中即时生效，仅改端口需先停止） */
async function onStrategyChange(strategy: string) {
  if (!detail.value || strategy === detail.value.strategy) return;
  switchingStrategy.value = true;
  try {
    detail.value = await updateAggregator({
      id: detail.value.id,
      name: detail.value.name,
      port: detail.value.port,
      tokenThreshold: detail.value.token_threshold,
      strategy,
    });
    message.success(`路由策略已切换为${strategyLabel(strategy)}`);
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    switchingStrategy.value = false;
  }
}

// ---- 手动切换当前计划（两种策略均可用） ----
const settingCurrent = ref(false);

/** 当前计划下拉选项：仅已绑定且启用的计划可设为当前；
 *  阈值轮转禁用已达阈值的选项（选了也会被轮转跳过） */
const currentPlanOptions = computed(() => {
  if (!detail.value) return [];
  const match = detail.value.strategy === STRATEGY_MODEL_MATCH;
  return detail.value.bindings
    .filter((b) => b.enabled)
    .map((b) => ({
      value: b.plan_id,
      disabled: !match && b.used_tokens >= b.token_threshold,
      label: match ? b.plan_name + (b.models.length ? `（${b.models.join("、")}）` : "") : b.plan_name,
    }));
});

async function onSetCurrentPlan(planId: number | null) {
  if (!detail.value) return;
  settingCurrent.value = true;
  try {
    detail.value = await setAggregatorCurrentPlan(detail.value.id, planId);
    message.success(planId == null ? "已恢复自动轮转" : "当前计划已切换");
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    settingCurrent.value = false;
  }
}

async function resetUsage() {
  if (!detail.value) return;
  resetting.value = true;
  try {
    detail.value = await resetAggregatorUsage(detail.value.id);
    message.success("用量已清零");
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    resetting.value = false;
  }
}

async function saveBindings(planIds: number[]) {
  if (!detail.value) return;
  try {
    detail.value = await setAggregatorPlans(detail.value.id, planIds);
    await refresh();
  } catch (e) {
    message.error(String(e));
  }
}

function moveBinding(index: number, dir: -1 | 1) {
  if (!detail.value) return;
  const ids = detail.value.bindings.map((b) => b.plan_id);
  const j = index + dir;
  if (j < 0 || j >= ids.length) return;
  [ids[index], ids[j]] = [ids[j], ids[index]];
  saveBindings(ids);
}

function removeBinding(index: number) {
  if (!detail.value) return;
  const ids = detail.value.bindings.map((b) => b.plan_id);
  ids.splice(index, 1);
  saveBindings(ids);
}

function addBinding() {
  if (!detail.value || !planToAdd.value) return;
  const ids = detail.value.bindings.map((b) => b.plan_id);
  ids.push(planToAdd.value);
  planToAdd.value = null;
  saveBindings(ids);
}

// 实时刷新（有新消息时更新统计，防抖避免请求密集时全量查询风暴）
let unlisten: (() => void) | null = null;
onMounted(async () => {
  await refresh();
  unlisten = await onNewMessage(debounce(() => refresh(), 300));
});
onUnmounted(() => unlisten?.());
</script>
