<template>
  <div>
    <a-flex justify="space-between" align="center" style="margin-bottom: 12px">
      <a-typography-title :level="4" style="margin: 0">配置</a-typography-title>
    </a-flex>

    <a-card title="窗口与托盘" size="small" style="margin-bottom: 16px">
      <a-flex justify="space-between" align="center" :gap="16">
        <div>
          <a-typography-text>关闭时隐藏到托盘</a-typography-text>
          <a-typography-text type="secondary" class="setting-note">
            开启后可通过托盘恢复主窗口；关闭后，退出程序前会弹出确认框。
          </a-typography-text>
        </div>
        <a-switch
          :checked="hideOnClose"
          @update:checked="emit('update:hideOnClose', $event === true)"
        />
      </a-flex>
    </a-card>

    <a-card title="数据库" size="small">
      <template #extra>
        <a-button size="small" :loading="dbLoading" @click="loadDbInfo">刷新</a-button>
      </template>
      <a-spin :spinning="dbLoading && !dbInfo" size="small">
        <a-alert
          v-if="dbError"
          :message="dbError"
          type="warning"
          show-icon
          style="margin-bottom: 12px"
        >
          <template #action>
            <a-button size="small" @click="loadDbInfo">重试</a-button>
          </template>
        </a-alert>

        <template v-if="dbInfo">
          <a-descriptions :column="2" size="small">
            <a-descriptions-item label="数据库路径" :span="2">
              <a-flex align="flex-start" :gap="4" class="db-path">
                <a-typography-text code :title="dbInfo.db_path" class="db-path-text">
                  {{ dbInfo.db_path }}
                </a-typography-text>
                <a-tooltip title="复制路径">
                  <a-button
                    type="text"
                    size="small"
                    class="db-copy"
                    @click="copyText(dbInfo.db_path, '数据库路径已复制')"
                  >
                    <CopyOutlined />
                  </a-button>
                </a-tooltip>
              </a-flex>
            </a-descriptions-item>
            <a-descriptions-item label="主库大小">
              <a-tooltip :title="`${formatNumber(dbInfo.db_size)} 字节`">
                <span>{{ formatBytes(dbInfo.db_size) }}</span>
              </a-tooltip>
            </a-descriptions-item>
            <a-descriptions-item label="文件总大小">
              <a-tooltip
                :title="`${formatNumber(dbInfo.total_size)} 字节（主库 + WAL + SHM 文件长度之和）`"
              >
                <span>{{ formatBytes(dbInfo.total_size) }}</span>
              </a-tooltip>
            </a-descriptions-item>
            <a-descriptions-item label="辅助文件" :span="2">
              <a-typography-text type="secondary">
                WAL {{ formatBytes(dbInfo.wal_size) }} · SHM {{ formatBytes(dbInfo.shm_size) }}
              </a-typography-text>
            </a-descriptions-item>
          </a-descriptions>
          <a-typography-text type="secondary" class="db-note">
            文件总大小为主库、WAL、SHM 的文件长度之和，不代表文件系统实际分配空间。
          </a-typography-text>
        </template>
      </a-spin>
    </a-card>

    <a-card title="空间维护" size="small" style="margin-top: 16px">
      <a-alert
        v-if="lastResult && !resultDismissed"
        :type="resultAlertType"
        :message="resultTitle"
        :description="lastResult.message"
        show-icon
        closable
        class="result-alert"
        @close="resultDismissed = true"
      />

      <a-flex justify="space-between" align="center" :gap="16" class="maint-row">
        <div>
          <a-typography-text>压缩历史数据</a-typography-text>
          <a-typography-text type="secondary" class="setting-note">
            {{ compressHint }}
          </a-typography-text>
        </div>
        <a-button
          type="primary"
          size="small"
          :disabled="!canCompress"
          :loading="compressBusy"
          @click="confirmCompress"
        >
          压缩历史数据
        </a-button>
      </a-flex>

      <a-flex justify="space-between" align="center" :gap="16">
        <div>
          <a-typography-text>整理文件</a-typography-text>
          <a-typography-text type="secondary" class="setting-note">
            {{ vacuumHint }}
          </a-typography-text>
        </div>
        <a-button size="small" :disabled="!canVacuum" :loading="vacuumBusy" @click="confirmVacuum">
          整理文件
        </a-button>
      </a-flex>

      <div v-if="job" class="job-box">
        <a-progress
          v-if="job.kind === 'compress'"
          :percent="Math.round(job.fraction * 100)"
          size="small"
          :status="job.cancelling ? 'normal' : 'active'"
        />
        <a-flex v-else align="center" :gap="8">
          <a-spin size="small" />
          <span>{{ job.stage }}</span>
        </a-flex>
        <a-flex justify="space-between" align="center" class="job-meta">
          <a-typography-text type="secondary">
            <template v-if="job.kind === 'compress'">
              {{ job.stage }} · {{ formatBytes(job.bytes_in) }} → {{ formatBytes(job.bytes_out) }}
              ·
            </template>
            已用时 {{ jobDuration(job.elapsed_ms) }}
          </a-typography-text>
          <a-button type="link" size="small" :disabled="job.cancelling" @click="cancelJob">
            {{ job.cancelling ? "正在取消…" : "取消" }}
          </a-button>
        </a-flex>
      </div>
    </a-card>
  </div>
</template>

<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { CopyOutlined } from "@ant-design/icons-vue";
import { Modal, message } from "ant-design-vue";
import {
  cancelMaintenance,
  getDbInfo,
  startCompressMessages,
  startVacuum,
} from "../api";
import type { DbInfo } from "../types";
import { copyText, formatBytes, formatDuration, formatNumber } from "../utils";

defineProps<{ hideOnClose: boolean }>();
const emit = defineEmits<{
  "update:hideOnClose": [value: boolean];
}>();

const dbInfo = ref<DbInfo | null>(null);
const dbLoading = ref(false);
const dbError = ref("");

async function loadDbInfo() {
  dbLoading.value = true;
  dbError.value = "";
  try {
    dbInfo.value = await getDbInfo();
  } catch (e) {
    // 加载失败时保留旧数据，仅在卡片内提示
    dbError.value = String(e);
  } finally {
    dbLoading.value = false;
  }
  syncPolling();
}

// 有维护任务时每秒静默轮询（进度 / 已用时 / 结局）；任务结束后再刷新一次拿最终统计
const POLL_MS = 1000;
let pollTimer: ReturnType<typeof setInterval> | null = null;

function syncPolling() {
  if (job.value) {
    if (pollTimer == null) pollTimer = setInterval(pollDbInfo, POLL_MS);
  } else if (pollTimer != null) {
    clearInterval(pollTimer);
    pollTimer = null;
    void loadDbInfo();
  }
}

async function pollDbInfo() {
  try {
    dbInfo.value = await getDbInfo();
  } catch {
    // 单拍失败（数据库忙等）保留旧数据，下一拍再试
  }
  syncPolling();
}

onMounted(loadDbInfo);
onUnmounted(() => {
  if (pollTimer != null) clearInterval(pollTimer);
});

// ---------------------------------------------------------------------------
// 空间维护
// ---------------------------------------------------------------------------

/** 低于此值的空闲页不值得整理（VACUUM 重写全库，几分钟锁库换几 MB 不划算） */
const MIN_VACUUM_BYTES = 64 * 1024 * 1024;

const job = computed(() => dbInfo.value?.job ?? null);
const lastResult = computed(() => dbInfo.value?.last_result ?? null);
const legacyRows = computed(() => dbInfo.value?.legacy_rows ?? null);
const reclaimable = computed(() => dbInfo.value?.reclaimable_bytes ?? null);
const aggregatorsRunning = computed(() => (dbInfo.value?.running_aggregators ?? 0) > 0);

const resultDismissed = ref(false);
watch(lastResult, () => {
  resultDismissed.value = false;
});

const resultAlertType = computed<"success" | "warning" | "error">(() =>
  lastResult.value?.outcome === "completed"
    ? "success"
    : lastResult.value?.outcome === "cancelled"
      ? "warning"
      : "error",
);

/** 维护任务用时：小库的整理不到一秒就完，直说而不是「用时 0 秒」 */
function jobDuration(ms: number) {
  return ms > 0 && ms < 1000 ? "不足 1 秒" : formatDuration(ms);
}

const resultTitle = computed(() => {
  const r = lastResult.value;
  if (!r) return "";
  const kind = r.kind === "compress" ? "压缩历史数据" : "整理文件";
  const outcome =
    r.outcome === "completed" ? "完成" : r.outcome === "cancelled" ? "已取消" : "失败";
  return `${kind}${outcome}（用时 ${jobDuration(r.elapsed_ms)}）`;
});

const compressHint = computed(() => {
  if (job.value) return job.value.kind === "compress" ? "任务进行中" : "已有维护任务正在进行";
  if (legacyRows.value == null) return "暂时无法读取（数据库忙）";
  if (legacyRows.value === 0) return "全部消息已压缩存储";
  return `${formatNumber(legacyRows.value)} 条消息仍为未压缩存储。无损压缩，体积通常降到原来的 10% 左右；后台进行，可取消、可续跑`;
});

const vacuumHint = computed(() => {
  if (job.value) return job.value.kind === "vacuum" ? "任务进行中" : "已有维护任务正在进行";
  if (reclaimable.value == null) return "暂时无法读取（数据库忙）";
  if (aggregatorsRunning.value) {
    return `需先停止全部运行中的聚合器（当前 ${dbInfo.value?.running_aggregators} 个在运行）`;
  }
  if (reclaimable.value < MIN_VACUUM_BYTES) return "没有值得回收的空闲页";
  return `可回收 ${formatBytes(reclaimable.value)}，整理后数据库约 ${formatBytes(dbInfo.value?.estimated_size)}`;
});

const canCompress = computed(() => !job.value && (legacyRows.value ?? 0) > 0);
const canVacuum = computed(
  () =>
    !job.value &&
    !aggregatorsRunning.value &&
    (reclaimable.value ?? 0) >= MIN_VACUUM_BYTES,
);

const compressBusy = ref(false);
const vacuumBusy = ref(false);

function confirmCompress() {
  // 粗估上限：耗时基本只由待压缩的体积决定，实测约 1.8 GiB/分钟（有聚合器运行时
  // 分小批让出数据库锁，会更慢）；未压缩行只来自旧版本，故按整库大小估
  const minutes = Math.max(1, Math.ceil(((dbInfo.value?.db_size ?? 0) / 1024 ** 3) * 0.6));
  Modal.confirm({
    title: "压缩历史数据",
    content: `将对 ${formatNumber(legacyRows.value ?? 0)} 条历史消息做无损压缩，内容逐字节保留。任务在后台进行，按当前库大小预计 ${minutes} 分钟上下；期间聚合器的请求可能要多等片刻（通常不到半秒）。可随时取消，再次压缩会从剩余部分继续。`,
    okText: "开始压缩",
    cancelText: "取消",
    onOk: runCompress,
  });
}

async function runCompress() {
  if (compressBusy.value) return;
  compressBusy.value = true;
  resultDismissed.value = false;
  try {
    await startCompressMessages();
    await loadDbInfo();
  } catch (e) {
    message.error(String(e));
  } finally {
    compressBusy.value = false;
  }
}

function confirmVacuum() {
  const info = dbInfo.value;
  if (!info) return;
  // 粗估：耗时只与整理后的体积（有效数据量）成正比、与当前文件大小无关，实测约
  // 25 秒/GiB，按 0.4 分钟/GiB 留余量；临时库与 WAL 各约一份整理后大小
  const estimated = info.estimated_size ?? Math.max(0, info.db_size - (reclaimable.value ?? 0));
  const minutes = Math.max(1, Math.ceil((estimated / 1024 ** 3) * 0.4));
  Modal.confirm({
    title: "整理文件",
    content: `把 ${formatBytes(reclaimable.value)} 空闲页归还给文件系统，数据库文件预计从 ${formatBytes(info.db_size)} 缩小到约 ${formatBytes(estimated)}。整理期间数据库完全锁定（消息列表、统计暂不可用），预计约 ${minutes} 分钟，额外需要约 ${formatBytes(estimated * 2)} 的临时磁盘空间。可随时取消（最后的回写阶段要等写完才响应，通常十几秒），取消不会损坏数据。`,
    okText: "开始整理",
    cancelText: "取消",
    onOk: runVacuum,
  });
}

async function runVacuum() {
  if (vacuumBusy.value) return;
  vacuumBusy.value = true;
  resultDismissed.value = false;
  try {
    await startVacuum();
    await loadDbInfo();
  } catch (e) {
    message.error(String(e));
  } finally {
    vacuumBusy.value = false;
  }
}

async function cancelJob() {
  try {
    await cancelMaintenance();
    await loadDbInfo();
  } catch (e) {
    message.error(String(e));
  }
}
</script>

<style scoped>
.setting-note {
  display: block;
  margin-top: 4px;
  font-size: 12px;
}
.db-path {
  min-width: 0;
}
.db-path-text {
  min-width: 0;
  overflow-wrap: anywhere;
  word-break: break-all;
}
.db-copy {
  flex: none;
}
.db-note {
  display: block;
  margin-top: 8px;
  font-size: 12px;
}
.result-alert {
  margin-bottom: 12px;
}
.maint-row {
  margin-bottom: 12px;
}
.job-box {
  margin-top: 12px;
  padding: 8px 12px;
  border-radius: 6px;
  background: rgba(0, 0, 0, 0.02);
}
.job-meta {
  margin-top: 4px;
  font-size: 12px;
}
</style>
