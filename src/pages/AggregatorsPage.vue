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
          <a-tag v-for="b in record.bindings" :key="b.plan_id" style="margin: 2px">
            {{ b.plan_name }}
            <a-tooltip :title="`已用 ${formatNumber(b.used_tokens)} / 阈值 ${formatNumber(b.token_threshold)}`">
              <InfoCircleOutlined style="margin-left: 4px" />
            </a-tooltip>
          </a-tag>
          <a-tag v-if="!record.bindings.length" color="warning">未绑定</a-tag>
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
        <a-form-item label="单计划 token 阈值" extra="某个计划消耗达到阈值后自动切换到下一个；全部达到后清零回绕">
          <a-input-number
            v-model:value="form.token_threshold"
            :min="1"
            :step="100000"
            style="width: 100%"
          />
        </a-form-item>
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
          <a-flex align="center" :gap="8">
            <a-tag color="blue">阈值轮转 threshold_rotation</a-tag>
            <span>单计划阈值</span>
            <a-input-number
              v-model:value="thresholdDraft"
              :min="1"
              :step="100000"
              size="small"
              style="width: 140px"
            />
            <a-button size="small" @click="saveThreshold" :loading="savingThreshold">保存</a-button>
            <a-button size="small" @click="resetUsage" :loading="resetting">重置用量</a-button>
          </a-flex>
        </a-card>

        <a-card size="small" title="绑定的 Coding Plan（自上而下轮转）">
          <a-table
            :columns="bindingColumns"
            :data-source="detail.bindings"
            :pagination="false"
            size="small"
            row-key="plan_id"
          >
            <template #bodyCell="{ column, record, index }">
              <template v-if="column.key === 'usage'">
                <a-progress
                  :percent="Math.min(100, Math.round((record.used_tokens / Math.max(1, record.token_threshold)) * 100))"
                  :status="record.used_tokens >= record.token_threshold ? 'exception' : 'active'"
                  size="small"
                  :format="() => `${formatNumber(record.used_tokens)} / ${formatNumber(record.token_threshold)}`"
                />
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
  CopyOutlined,
  DeleteOutlined,
  DownOutlined,
  EyeInvisibleOutlined,
  EyeOutlined,
  InfoCircleOutlined,
  UpOutlined,
} from "@ant-design/icons-vue";
import {
  createAggregator,
  deleteAggregator,
  listAggregators,
  listPlans,
  resetAggregatorUsage,
  setAggregatorPlans,
  startAggregator,
  stopAggregator,
  updateAggregator,
  onNewMessage,
} from "../api";
import type { AggregatorView } from "../types";
import { copyText, debounce, formatNumber } from "../utils";

const columns = [
  { title: "名称", dataIndex: "name", key: "name", width: 140 },
  { title: "状态", key: "status", width: 90 },
  { title: "BASE_URL", key: "base_url" },
  { title: "AUTH_TOKEN", key: "auth_token" },
  { title: "绑定计划", key: "plans" },
  { title: "累计消耗", key: "stats", width: 180 },
  { title: "操作", key: "actions", width: 260 },
];

const bindingColumns = [
  { title: "顺序", dataIndex: "position", key: "position", width: 60 },
  { title: "计划", dataIndex: "plan_name", key: "plan_name" },
  { title: "已用 / 阈值", key: "usage" },
  { title: "操作", key: "actions", width: 130 },
];

const aggs = ref<AggregatorView[]>([]);
const loading = ref(false);
const showToken = reactive<Record<number, boolean>>({});
const toggling = reactive<Record<number, boolean>>({});

const maskToken = (t: string) => (t.length > 8 ? `${t.slice(0, 5)}••••••${t.slice(-4)}` : "••••");

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
const form = reactive<{ name: string; port: number | null; token_threshold: number }>({
  name: "",
  port: null,
  token_threshold: 1000000,
});

function openCreate() {
  editing.value = null;
  Object.assign(form, { name: "", port: null, token_threshold: 1000000 });
  modalOpen.value = true;
}

function openEdit(agg: AggregatorView) {
  editing.value = agg;
  Object.assign(form, {
    name: agg.name,
    port: agg.port,
    token_threshold: agg.token_threshold,
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
      });
      message.success("已更新");
    } else {
      await createAggregator({
        name: form.name,
        port: form.port,
        tokenThreshold: form.token_threshold,
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
const thresholdDraft = ref<number>(0);
const savingThreshold = ref(false);
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
    });
    message.success("阈值已保存");
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    savingThreshold.value = false;
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
