<template>
  <div>
    <a-flex justify="space-between" align="center" style="margin-bottom: 12px">
      <a-typography-title :level="4" style="margin: 0">统计</a-typography-title>
      <a-flex :gap="8" align="center">
        <a-select
          v-model:value="filterAgg"
          placeholder="全部聚合器"
          allow-clear
          style="width: 180px"
          :options="aggOptions"
          @change="reloadAll"
        />
        <a-button @click="reloadAll">刷新</a-button>
      </a-flex>
    </a-flex>

    <a-card title="按天统计" size="small" style="margin-bottom: 16px">
      <template #extra>
        <a-radio-group v-model:value="days" size="small" @change="loadDaily">
          <a-radio-button :value="7">近 7 天</a-radio-button>
          <a-radio-button :value="14">近 14 天</a-radio-button>
          <a-radio-button :value="30">近 30 天</a-radio-button>
        </a-radio-group>
      </template>
      <TokenBars :bars="dailyBars" :height="220" />
      <div class="legend">
        <span><i class="dot prompt" /> 输入 Token</span>
        <span><i class="dot completion" /> 输出 Token</span>
      </div>
    </a-card>

    <a-card title="按小时统计" size="small" style="margin-bottom: 16px">
      <template #extra>
        <a-date-picker v-model:value="hourDate" size="small" :allow-clear="false" @change="loadHourly" />
      </template>
      <TokenBars :bars="hourlyBars" :height="220" />
      <div class="legend">
        <span><i class="dot prompt" /> 输入 Token</span>
        <span><i class="dot completion" /> 输出 Token</span>
      </div>
    </a-card>

    <a-card title="按模型统计" size="small">
      <template #extra>
        <a-radio-group v-model:value="modelDays" size="small" @change="loadModels">
          <a-radio-button :value="0">全部</a-radio-button>
          <a-radio-button :value="7">近 7 天</a-radio-button>
          <a-radio-button :value="14">近 14 天</a-radio-button>
          <a-radio-button :value="30">近 30 天</a-radio-button>
        </a-radio-group>
      </template>
      <a-table
        :columns="modelColumns"
        :data-source="modelRows"
        :loading="loading"
        row-key="key"
        size="middle"
        :pagination="false"
      >
        <template #bodyCell="{ column, record }">
          <template v-if="column.key === 'model'">
            {{ record.key || "（未知）" }}
          </template>
          <template v-else-if="column.key === 'total'">
            {{ formatNumber(record.total_tokens) }}
          </template>
          <template v-else-if="column.key === 'prompt'">
            {{ formatNumber(record.prompt_tokens) }}
          </template>
          <template v-else-if="column.key === 'completion'">
            {{ formatNumber(record.completion_tokens) }}
          </template>
          <template v-else-if="column.key === 'share'">
            <a-progress
              :percent="record.share"
              :stroke-color="{ from: '#1677ff', to: '#69b1ff' }"
              size="small"
              style="margin: 0"
            />
          </template>
        </template>
      </a-table>
    </a-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { message } from "ant-design-vue";
import dayjs, { type Dayjs } from "dayjs";
import TokenBars, { type BarItem } from "../components/TokenBars.vue";
import { dailyStats, hourlyStats, listAggregators, modelStats, onNewMessage } from "../api";
import type { StatsBucket } from "../types";
import { formatNumber } from "../utils";

const aggOptions = ref<{ value: number; label: string }[]>([]);
const filterAgg = ref<number | null>(null);
const loading = ref(false);

// ---- 按天 ----
const days = ref(30);
const daily = ref<StatsBucket[]>([]);

/** 近 N 天日期序列（补零），key 为 "YYYY-MM-DD" */
function lastNDates(n: number): string[] {
  const today = dayjs();
  return Array.from({ length: n }, (_, i) =>
    today.subtract(n - 1 - i, "day").format("YYYY-MM-DD"),
  );
}

const dailyBars = computed<BarItem[]>(() => {
  const byKey = new Map(daily.value.map((b) => [b.key, b]));
  return lastNDates(days.value).map((d) => {
    const b = byKey.get(d);
    const prompt = b?.prompt_tokens ?? 0;
    const completion = b?.completion_tokens ?? 0;
    const total = b?.total_tokens ?? 0;
    const dateLabel = d.slice(5); // MM-DD
    return {
      label: dateLabel,
      value: total,
      promptValue: prompt,
      completionValue: completion,
      tooltip: `${d}｜总量 ${formatNumber(total)}（入 ${formatNumber(prompt)} / 出 ${formatNumber(completion)}）｜${b?.requests ?? 0} 次请求`,
    };
  });
});

async function loadDaily() {
  try {
    daily.value = await dailyStats({ aggregatorId: filterAgg.value, days: days.value });
  } catch (e) {
    message.error(String(e));
  }
}

// ---- 按小时 ----
const hourDate = ref<Dayjs>(dayjs());
const hourly = ref<StatsBucket[]>([]);

const hourlyBars = computed<BarItem[]>(() => {
  const byKey = new Map(hourly.value.map((b) => [b.key, b]));
  return Array.from({ length: 24 }, (_, h) => {
    const key = String(h).padStart(2, "0");
    const b = byKey.get(key);
    const prompt = b?.prompt_tokens ?? 0;
    const completion = b?.completion_tokens ?? 0;
    const total = b?.total_tokens ?? 0;
    return {
      label: key,
      value: total,
      promptValue: prompt,
      completionValue: completion,
      tooltip: `${key}:00-${key}:59｜总量 ${formatNumber(total)}（入 ${formatNumber(prompt)} / 出 ${formatNumber(completion)}）｜${b?.requests ?? 0} 次请求`,
    };
  });
});

async function loadHourly() {
  try {
    hourly.value = await hourlyStats({
      aggregatorId: filterAgg.value,
      date: hourDate.value.format("YYYY-MM-DD"),
    });
  } catch (e) {
    message.error(String(e));
  }
}

// ---- 按模型 ----
const models = ref<StatsBucket[]>([]);
/** 时间段：0 = 全部时间，>0 = 近 N 天 */
const modelDays = ref(0);

const modelColumns = [
  { title: "模型", key: "model" },
  { title: "总 Token", key: "total", width: 140 },
  { title: "输入", key: "prompt", width: 120 },
  { title: "输出", key: "completion", width: 120 },
  { title: "请求数", dataIndex: "requests", key: "requests", width: 90 },
  { title: "占比", key: "share", width: 220 },
];

const modelRows = computed(() =>
  models.value.map((b) => ({
    ...b,
    share: b.total_tokens > 0 && models.value.length > 0
      ? Math.round((b.total_tokens / models.value.reduce((s, x) => s + x.total_tokens, 0)) * 1000) / 10
      : 0,
  })),
);

async function loadModels() {
  try {
    models.value = await modelStats({
      aggregatorId: filterAgg.value,
      days: modelDays.value > 0 ? modelDays.value : null,
    });
  } catch (e) {
    message.error(String(e));
  }
}

function reloadAll() {
  loading.value = true;
  Promise.all([loadDaily(), loadHourly(), loadModels()]).finally(() => (loading.value = false));
}

let unlisten: (() => void) | null = null;
onMounted(async () => {
  reloadAll();
  listAggregators().then((aggs) => {
    aggOptions.value = aggs.map((a) => ({ value: a.id, label: a.name }));
  });
  unlisten = await onNewMessage(() => reloadAll());
});
onUnmounted(() => unlisten?.());
</script>

<style scoped>
.legend {
  display: flex;
  gap: 16px;
  justify-content: flex-end;
  font-size: 12px;
  color: #666;
  margin-top: 4px;
}
.dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 2px;
  margin-right: 4px;
}
.dot.prompt {
  background: #1677ff;
}
.dot.completion {
  background: #69b1ff;
}
</style>
