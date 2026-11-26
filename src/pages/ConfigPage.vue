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
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from "vue";
import { CopyOutlined } from "@ant-design/icons-vue";
import { getDbInfo } from "../api";
import type { DbInfo } from "../types";
import { copyText, formatBytes, formatNumber } from "../utils";

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
}

onMounted(loadDbInfo);
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
</style>
