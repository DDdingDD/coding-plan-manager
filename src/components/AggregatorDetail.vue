<template>
  <div>
    <a-card size="small" title="接入参数" style="margin-bottom: 16px">
      <a-descriptions :column="1" size="small">
        <a-descriptions-item label="BASE_URL">
          <a-flex :gap="6" align="center">
            <a-typography-text code>{{ agg.base_url }}</a-typography-text>
            <a-button size="small" type="text" @click="copyText(agg.base_url, '已复制')">
              <CopyOutlined />
            </a-button>
          </a-flex>
        </a-descriptions-item>
        <a-descriptions-item label="AUTH_TOKEN">
          <a-flex :gap="6" align="center">
            <a-typography-text code>{{ agg.auth_token }}</a-typography-text>
            <a-button size="small" type="text" @click="copyText(agg.auth_token, '已复制')">
              <CopyOutlined />
            </a-button>
          </a-flex>
        </a-descriptions-item>
      </a-descriptions>
      <a-typography-paragraph type="secondary" style="margin-bottom: 0; font-size: 12px">
        Claude Code 接入：设置环境变量 ANTHROPIC_BASE_URL={{ agg.base_url }}、
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
          {{ agg.strategy === "model_match" ? "无匹配模型时走此计划，匹配命中即自动切换" : "下一请求将转发到此计划" }}
        </a-typography-text>
      </a-flex>
      <a-flex align="center" :gap="8" style="margin-bottom: 8px">
        <span>当前计划：</span>
        <a-select
          v-if="agg.strategy === 'model_match'"
          :value="agg.current_plan_id"
          style="width: 220px"
          :options="currentPlanOptions"
          :loading="settingCurrent"
          @change="(v: any) => onSetCurrentPlan(v as number)"
        />
        <a-select
          v-else
          :value="agg.manual_current_plan_id ?? undefined"
          style="width: 220px"
          :options="currentPlanOptions"
          :loading="settingCurrent"
          allow-clear
          placeholder="自动轮转"
          @change="(v: any) => onSetCurrentPlan((v ?? null) as number | null)"
        />
        <a-typography-text type="secondary" style="font-size: 12px">
          {{
            agg.strategy === "model_match"
              ? "手动指定下一请求的兜底计划；模型匹配命中时会自动切换"
              : "固定当前转发的计划；该计划达阈值或失效后自动恢复轮转，清空则回到自动"
          }}
        </a-typography-text>
      </a-flex>
      <a-flex align="center" :gap="8">
        <a-radio-group
          :value="agg.strategy"
          size="small"
          button-style="solid"
          :disabled="switchingStrategy"
          @change="(e: any) => onStrategyChange(e.target.value as string)"
        >
          <a-radio-button value="threshold_rotation">阈值轮转</a-radio-button>
          <a-radio-button value="model_match">模型匹配</a-radio-button>
        </a-radio-group>
        <template v-if="agg.strategy === 'threshold_rotation'">
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
      :title="agg.strategy === 'model_match' ? '绑定的 Coding Plan（按模型匹配路由）' : '绑定的 Coding Plan（自上而下轮转）'"
    >
      <a-table
        :columns="bindingColumns"
        :data-source="agg.bindings"
        :pagination="false"
        size="small"
        row-key="plan_id"
      >
        <template #bodyCell="{ column, record, index }">
          <template v-if="column.key === 'plan_name'">
            {{ record.plan_name }}
            <a-tag
              v-if="record.plan_id === agg.current_plan_id"
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
                :disabled="index === agg.bindings.length - 1"
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
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { message } from "ant-design-vue";
import {
  AimOutlined,
  CopyOutlined,
  DeleteOutlined,
  DownOutlined,
  UpOutlined,
} from "@ant-design/icons-vue";
import {
  listPlans,
  resetAggregatorUsage,
  setAggregatorCurrentPlan,
  setAggregatorPlans,
  updateAggregator,
} from "../api";
import type { AggregatorView } from "../types";
import { copyText, formatNumber } from "../utils";

/** 路由策略标识（与 Rust 侧 db.rs 常量一致） */
const STRATEGY_MODEL_MATCH = "model_match";

const strategyLabel = (s: string) =>
  s === STRATEGY_MODEL_MATCH ? "模型匹配" : "阈值轮转";

const props = defineProps<{ agg: AggregatorView }>();
/** 任何修改成功后把后端返回的最新视图抛给父组件（父组件就地更新并刷新列表） */
const emit = defineEmits<{ (e: "changed", agg: AggregatorView): void }>();

/** 当前转发计划（current_plan_id 对应绑定）的名称，无可用计划时为 null */
const currentPlanName = computed(
  () => props.agg.bindings.find((b) => b.plan_id === props.agg.current_plan_id)?.plan_name ?? null,
);

/** 绑定表格列：仅模型匹配策略显示「支持模型」（阈值轮转不依赖该列） */
const bindingColumns = computed(() => {
  const cols: { title: string; dataIndex?: string; key: string; width?: number }[] = [
    { title: "顺序", dataIndex: "position", key: "position", width: 60 },
    { title: "计划", dataIndex: "plan_name", key: "plan_name" },
  ];
  if (props.agg.strategy === STRATEGY_MODEL_MATCH) {
    cols.push({ title: "支持模型", key: "models", width: 160 });
  }
  cols.push(
    { title: "已用 / 阈值", key: "usage", width: 170 },
    { title: "操作", key: "actions", width: 120 },
  );
  return cols;
});

const thresholdDraft = ref<number>(0);
const savingThreshold = ref(false);
const switchingStrategy = ref(false);
const resetting = ref(false);
const planToAdd = ref<number | null>(null);
const allPlans = ref<{ id: number; name: string; enabled: boolean }[]>([]);

// 切换展示对象或阈值被外部修改时同步草稿
watch(
  () => [props.agg.id, props.agg.token_threshold],
  () => (thresholdDraft.value = props.agg.token_threshold),
  { immediate: true },
);

onMounted(() => {
  listPlans().then(
    (ps) => (allPlans.value = ps.map((p) => ({ id: p.id, name: p.name, enabled: p.enabled }))),
  );
});

const bindableOptions = computed(() => {
  const bound = new Set(props.agg.bindings.map((b) => b.plan_id));
  return allPlans.value
    .filter((p) => !bound.has(p.id))
    .map((p) => ({ value: p.id, label: p.name + (p.enabled ? "" : "（已禁用）") }));
});

async function saveThreshold() {
  savingThreshold.value = true;
  try {
    const updated = await updateAggregator({
      id: props.agg.id,
      name: props.agg.name,
      port: props.agg.port,
      tokenThreshold: thresholdDraft.value,
      strategy: props.agg.strategy,
    });
    message.success("阈值已保存");
    emit("changed", updated);
  } catch (e) {
    message.error(String(e));
  } finally {
    savingThreshold.value = false;
  }
}

/** 直接切换路由策略（配置逐请求重读，运行中即时生效，仅改端口需先停止） */
async function onStrategyChange(strategy: string) {
  if (strategy === props.agg.strategy) return;
  switchingStrategy.value = true;
  try {
    const updated = await updateAggregator({
      id: props.agg.id,
      name: props.agg.name,
      port: props.agg.port,
      tokenThreshold: props.agg.token_threshold,
      strategy,
    });
    message.success(`路由策略已切换为${strategyLabel(strategy)}`);
    emit("changed", updated);
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
  const match = props.agg.strategy === STRATEGY_MODEL_MATCH;
  return props.agg.bindings
    .filter((b) => b.enabled)
    .map((b) => ({
      value: b.plan_id,
      disabled: !match && b.used_tokens >= b.token_threshold,
      label: match ? b.plan_name + (b.models.length ? `（${b.models.join("、")}）` : "") : b.plan_name,
    }));
});

async function onSetCurrentPlan(planId: number | null) {
  settingCurrent.value = true;
  try {
    const updated = await setAggregatorCurrentPlan(props.agg.id, planId);
    message.success(planId == null ? "已恢复自动轮转" : "当前计划已切换");
    emit("changed", updated);
  } catch (e) {
    message.error(String(e));
  } finally {
    settingCurrent.value = false;
  }
}

async function resetUsage() {
  resetting.value = true;
  try {
    const updated = await resetAggregatorUsage(props.agg.id);
    message.success("用量已清零");
    emit("changed", updated);
  } catch (e) {
    message.error(String(e));
  } finally {
    resetting.value = false;
  }
}

async function saveBindings(planIds: number[]) {
  try {
    const updated = await setAggregatorPlans(props.agg.id, planIds);
    emit("changed", updated);
  } catch (e) {
    message.error(String(e));
  }
}

function moveBinding(index: number, dir: -1 | 1) {
  const ids = props.agg.bindings.map((b) => b.plan_id);
  const j = index + dir;
  if (j < 0 || j >= ids.length) return;
  [ids[index], ids[j]] = [ids[j], ids[index]];
  saveBindings(ids);
}

function removeBinding(index: number) {
  const ids = props.agg.bindings.map((b) => b.plan_id);
  ids.splice(index, 1);
  saveBindings(ids);
}

function addBinding() {
  if (!planToAdd.value) return;
  const ids = props.agg.bindings.map((b) => b.plan_id);
  ids.push(planToAdd.value);
  planToAdd.value = null;
  saveBindings(ids);
}
</script>
