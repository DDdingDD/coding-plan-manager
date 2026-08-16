<template>
  <div>
    <a-flex justify="space-between" align="center" style="margin-bottom: 12px">
      <a-typography-title :level="4" style="margin: 0">消息记录</a-typography-title>
      <a-flex :gap="8" align="center">
        <span>实时刷新</span>
        <a-switch v-model:checked="realtime" size="small" />
        <a-select
          v-model:value="filterAgg"
          placeholder="全部聚合器"
          allow-clear
          style="width: 180px"
          :options="aggOptions"
          @change="reload"
        />
        <a-button @click="reload">刷新</a-button>
        <a-popconfirm title="清空当前筛选范围内的消息记录？" @confirm="doClear">
          <a-button danger>清空</a-button>
        </a-popconfirm>
      </a-flex>
    </a-flex>

    <a-table
      :columns="columns"
      :data-source="page.items"
      :loading="loading"
      row-key="id"
      size="middle"
      :pagination="{
        current: page.page,
        pageSize: page.page_size,
        total: page.total,
        showSizeChanger: true,
        pageSizeOptions: ['20', '50', '100'],
        showTotal: (t: number) => `共 ${t} 条`,
        onChange: (p: number, s: number) => loadPage(p, s),
      }"
    >
      <template #bodyCell="{ column, record }">
        <template v-if="column.key === 'status'">
          <a-tag :color="statusColor(record.status)">{{ record.status }}</a-tag>
        </template>
        <template v-else-if="column.key === 'tokens'">
          {{ formatNumber(record.total_tokens) }}
          <span style="color: #999; font-size: 12px">
            (入 {{ formatNumber(record.prompt_tokens) }} / 出 {{ formatNumber(record.completion_tokens) }})
          </span>
        </template>
        <template v-else-if="column.key === 'req'">
          <span style="font-family: monospace; font-size: 12px">
            {{ record.method }} {{ record.path }}
          </span>
        </template>
        <template v-else-if="column.key === 'duration'">
          {{ record.duration_ms }} ms
        </template>
        <template v-else-if="column.key === 'actions'">
          <a-button size="small" @click="openDetail(record.id)">详情</a-button>
        </template>
      </template>
    </a-table>

    <a-drawer v-model:open="detailOpen" width="760" :title="`消息 #${detail?.id ?? ''}`">
      <template v-if="detail">
        <a-descriptions :column="2" size="small" bordered style="margin-bottom: 16px">
          <a-descriptions-item label="时间">{{ detail.created_at }}</a-descriptions-item>
          <a-descriptions-item label="状态">
            <a-tag :color="statusColor(detail.status)">{{ detail.status }}</a-tag>
          </a-descriptions-item>
          <a-descriptions-item label="聚合器">
            {{ detail.aggregator_name ?? `#${detail.aggregator_id}` }}
          </a-descriptions-item>
          <a-descriptions-item label="Coding Plan">
            {{ detail.plan_name ?? "（未使用）" }}
          </a-descriptions-item>
          <a-descriptions-item label="请求" :span="2">
            <span style="font-family: monospace">{{ detail.method }} {{ detail.path }}</span>
          </a-descriptions-item>
          <a-descriptions-item label="Token">
            总 {{ formatNumber(detail.total_tokens) }}｜入 {{ formatNumber(detail.prompt_tokens) }}｜出
            {{ formatNumber(detail.completion_tokens) }}
          </a-descriptions-item>
          <a-descriptions-item label="耗时">{{ detail.duration_ms }} ms</a-descriptions-item>
        </a-descriptions>

        <a-tabs>
          <a-tab-pane key="req" tab="请求体">
            <pre class="body-view">{{ prettyJson(detail.request_body) }}</pre>
          </a-tab-pane>
          <a-tab-pane key="resp" tab="响应体">
            <pre class="body-view">{{ prettyJson(detail.response_body) }}</pre>
          </a-tab-pane>
        </a-tabs>
      </template>
    </a-drawer>
  </div>
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { message } from "ant-design-vue";
import {
  clearMessages,
  getMessage,
  listAggregators,
  listMessages,
  onNewMessage,
} from "../api";
import type { MessagePage, MessageRow } from "../types";
import { formatNumber, prettyJson } from "../utils";

const columns = [
  { title: "时间", dataIndex: "created_at", key: "created_at", width: 160 },
  { title: "聚合器", dataIndex: "aggregator_name", key: "aggregator_name", width: 110 },
  { title: "计划", dataIndex: "plan_name", key: "plan_name", width: 110 },
  { title: "请求", key: "req", ellipsis: true },
  { title: "状态", key: "status", width: 80 },
  { title: "Token", key: "tokens", width: 220 },
  { title: "耗时", key: "duration", width: 90 },
  { title: "操作", key: "actions", width: 80 },
];

const page = ref<MessagePage>({ total: 0, page: 1, page_size: 20, items: [] });
const loading = ref(false);
const realtime = ref(true);
const filterAgg = ref<number | null>(null);
const aggOptions = ref<{ value: number; label: string }[]>([]);

const statusColor = (s: number) =>
  s >= 200 && s < 300 ? "green" : s === 401 ? "orange" : s >= 500 ? "red" : "default";

async function loadPage(p = 1, size?: number) {
  loading.value = true;
  try {
    page.value = await listMessages({
      aggregatorId: filterAgg.value,
      page: p,
      pageSize: size ?? page.value.page_size,
    });
  } catch (e) {
    message.error(String(e));
  } finally {
    loading.value = false;
  }
}

const reload = () => loadPage(1);

const detailOpen = ref(false);
const detail = ref<MessageRow | null>(null);

async function openDetail(id: number) {
  try {
    detail.value = await getMessage(id);
    detailOpen.value = true;
  } catch (e) {
    message.error(String(e));
  }
}

async function doClear() {
  try {
    const n = await clearMessages(filterAgg.value);
    message.success(`已清空 ${n} 条记录`);
    await loadPage(1);
  } catch (e) {
    message.error(String(e));
  }
}

let unlisten: (() => void) | null = null;
onMounted(async () => {
  await loadPage(1);
  listAggregators().then((aggs) => {
    aggOptions.value = aggs.map((a) => ({ value: a.id, label: a.name }));
  });
  unlisten = await onNewMessage(() => {
    if (realtime.value && page.value.page === 1) reload();
  });
});
onUnmounted(() => unlisten?.());
</script>

<style scoped>
.body-view {
  background: #fafafa;
  border: 1px solid #f0f0f0;
  border-radius: 6px;
  padding: 12px;
  max-height: 480px;
  overflow: auto;
  font-size: 12px;
  white-space: pre-wrap;
  word-break: break-all;
}
</style>
