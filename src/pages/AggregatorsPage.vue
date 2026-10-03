<template>
  <div>
    <a-flex justify="space-between" align="center" style="margin-bottom: 12px">
      <a-typography-title :level="4" style="margin: 0">Plan Aggregator</a-typography-title>
      <a-flex :gap="8" align="center">
        <ConfigButtons @imported="onImported" />
        <a-button type="primary" @click="openCreate">新建聚合器</a-button>
      </a-flex>
    </a-flex>

    <a-table
      :columns="columns"
      :data-source="aggs"
      :loading="loading"
      row-key="id"
      :pagination="false"
      size="middle"
      :custom-row="customRow"
      :row-class-name="rowClassName"
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

    <!-- 内联详情：聚合器不超过 INLINE_DETAIL_MAX 个时直接展示在表格下方，点击行或「详情」切换 -->
    <template v-if="inlineDetail">
      <a-divider orientation="left" style="margin: 20px 0 16px">
        详情：{{ inlineDetail.name }}
      </a-divider>
      <AggregatorDetail
        :key="`${inlineDetail.id}-${configVersion}`"
        :agg="inlineDetail"
        @changed="onDetailChanged"
      />
    </template>

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

    <!-- 详情抽屉：聚合器数量超过 INLINE_DETAIL_MAX 个时使用（内联详情的备选形态） -->
    <a-drawer v-model:open="detailOpen" width="640" :title="detail?.name ?? ''">
      <AggregatorDetail
        v-if="detail"
        :key="`${detail.id}-${configVersion}`"
        :agg="detail"
        @changed="onDetailChanged"
      />
    </a-drawer>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref } from "vue";
import { message } from "ant-design-vue";
import {
  AimOutlined,
  CopyOutlined,
  EyeInvisibleOutlined,
  EyeOutlined,
} from "@ant-design/icons-vue";
import {
  createAggregator,
  deleteAggregator,
  listAggregators,
  startAggregator,
  stopAggregator,
  updateAggregator,
  onNewMessage,
} from "../api";
import type { AggregatorView, BindingView } from "../types";
import { copyText, debounce, formatNumber } from "../utils";
import AggregatorDetail from "../components/AggregatorDetail.vue";
import ConfigButtons from "../components/ConfigButtons.vue";

/** 路由策略标识（与 Rust 侧 db.rs 常量一致） */
const STRATEGY_THRESHOLD_ROTATION = "threshold_rotation";
const STRATEGY_MODEL_MATCH = "model_match";

/** 聚合器数量不超过此值时，详情直接内联显示在表格下方（否则走抽屉） */
const INLINE_DETAIL_MAX = 3;

const columns = [
  { title: "名称", dataIndex: "name", key: "name", width: 140 },
  { title: "状态", key: "status", width: 90 },
  { title: "BASE_URL", key: "base_url" },
  { title: "AUTH_TOKEN", key: "auth_token" },
  { title: "绑定计划", key: "plans" },
  { title: "累计消耗", key: "stats", width: 180 },
  { title: "操作", key: "actions", width: 260 },
];

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

// ---- 内联详情选择 ----
/** 内联模式下选中的聚合器 id；未选中或已删除时回落到第一个 */
const selectedId = ref<number | null>(null);
const inlineMode = computed(() => aggs.value.length > 0 && aggs.value.length <= INLINE_DETAIL_MAX);
const inlineDetail = computed(() => {
  if (!inlineMode.value) return null;
  return aggs.value.find((a) => a.id === selectedId.value) ?? aggs.value[0];
});

/** 内联模式下点击行即切换详情对象 */
function customRow(record: AggregatorView) {
  return {
    onClick: () => {
      if (inlineMode.value) selectedId.value = record.id;
    },
    style: inlineMode.value ? "cursor: pointer" : "",
  };
}

function rowClassName(record: AggregatorView) {
  return record.id === inlineDetail.value?.id ? "agg-row-active" : "";
}

/** 详情组件内修改成功：就地更新列表与抽屉数据，再后台刷新保持一致 */
async function onDetailChanged(updated: AggregatorView) {
  const idx = aggs.value.findIndex((a) => a.id === updated.id);
  if (idx >= 0) aggs.value[idx] = updated;
  if (detail.value?.id === updated.id) detail.value = updated;
  await refresh();
}

/** 导入配置成功：刷新列表，并以 configVersion 递增强制详情组件重挂载——
 * 组件在 onMounted 缓存了可绑定计划列表，merge 新增计划 / replace 全量
 * 换 id 后旧缓存已失活，仅靠列表刷新覆盖不到 */
const configVersion = ref(0);
async function onImported() {
  configVersion.value++;
  selectedId.value = null;
  detailOpen.value = false;
  await refresh();
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
    if (selectedId.value === id) selectedId.value = null;
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

// ---- 详情（抽屉 / 内联共用同一组件） ----
const detailOpen = ref(false);
const detail = ref<AggregatorView | null>(null);

/** 内联模式直接切换选中项；否则打开抽屉 */
function openDetail(agg: AggregatorView) {
  if (inlineMode.value) {
    selectedId.value = agg.id;
    return;
  }
  detail.value = agg;
  detailOpen.value = true;
}

// 实时刷新（有新消息时更新统计，防抖避免请求密集时全量查询风暴）
let unlisten: (() => void) | null = null;
onMounted(async () => {
  await refresh();
  unlisten = await onNewMessage(debounce(() => refresh(), 1500));
});
onUnmounted(() => unlisten?.());
</script>

<style scoped>
/* 内联详情模式下高亮选中行 */
:deep(.agg-row-active) > td {
  background: #e6f4ff !important;
}
</style>
