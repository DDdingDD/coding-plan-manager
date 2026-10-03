<template>
  <a-flex :gap="8" align="center">
    <a-button :disabled="busy" @click="doExport">导出配置</a-button>
    <a-button :disabled="busy" @click="openImport">导入配置</a-button>

    <!-- 导入模式选择：合并（追加）/ 替换（清空后复现） -->
    <a-modal
      v-model:open="modalOpen"
      title="导入配置"
      :confirm-loading="importing"
      destroy-on-close
      @ok="confirmImport"
    >
      <a-radio-group
        v-model:value="mode"
        style="display: flex; flex-direction: column; gap: 12px; margin-top: 12px"
      >
        <a-radio value="merge">
          合并导入
          <span class="mode-desc">文件中的计划与聚合器作为新条目追加，现有配置不受影响；端口冲突自动分配新端口</span>
        </a-radio>
        <a-radio value="replace">
          替换导入
          <span class="mode-desc">清空现有全部计划与聚合器，按文件完整复现</span>
        </a-radio>
      </a-radio-group>
      <a-typography-paragraph
        v-if="mode === 'replace'"
        type="danger"
        style="margin: 12px 0 0; font-size: 12px"
      >
        将清空现有全部计划与聚合器（消息记录保留），并停止运行中的聚合器服务；导入后需手动重启所需服务
      </a-typography-paragraph>
    </a-modal>
  </a-flex>
</template>

<script setup lang="ts">
import { ref } from "vue";
import { Modal, message } from "ant-design-vue";
import { exportConfig, importConfig } from "../api";

/** 导入成功后通知父页面刷新列表 */
const emit = defineEmits<{ (e: "imported"): void }>();

/** 文件对话框打开期间 WebView 的 JS 仍可响应，禁用按钮防止重复触发命令 */
const busy = ref(false);
const modalOpen = ref(false);
const importing = ref(false);
const mode = ref<"merge" | "replace">("merge");

async function doExport() {
  if (busy.value) return;
  busy.value = true;
  try {
    const path = await exportConfig();
    if (path == null) return; // 用户取消文件选择，静默
    message.success(`配置已导出到：${path}（文件包含令牌等敏感信息，请妥善保管）`);
  } catch (e) {
    message.error(String(e));
  } finally {
    busy.value = false;
  }
}

function openImport() {
  if (busy.value) return;
  mode.value = "merge";
  modalOpen.value = true;
}

function confirmImport() {
  modalOpen.value = false;
  if (mode.value === "replace") {
    // 替换导入破坏性大，模式选择后再做一次醒目确认
    Modal.confirm({
      title: "替换导入确认",
      content:
        "将清空现有全部计划与聚合器（消息记录保留），并停止运行中的聚合器服务。此操作不可撤销，确定继续？",
      okText: "清空并导入",
      okType: "danger",
      cancelText: "取消",
      onOk: () => runImport("replace"),
    });
  } else {
    runImport("merge");
  }
}

async function runImport(m: "merge" | "replace") {
  if (busy.value) return;
  busy.value = true;
  importing.value = true;
  try {
    const r = await importConfig(m);
    if (r == null) return; // 用户取消文件选择，静默
    let text = `导入完成：新增计划 ${r.plans_imported} 个、聚合器 ${r.aggregators_imported} 个`;
    if (r.stopped_servers > 0) text += `，已停止 ${r.stopped_servers} 个运行中的服务`;
    if (r.port_changes.length) {
      const changes = r.port_changes
        .map((c) => `${c.name} ${c.old_port}→${c.new_port}`)
        .join("、");
      text += `；端口冲突已自动分配：${changes}`;
    }
    message.success(text);
    emit("imported");
  } catch (e) {
    message.error(String(e));
  } finally {
    busy.value = false;
    importing.value = false;
  }
}
</script>

<style scoped>
.mode-desc {
  display: block;
  margin-left: 24px;
  color: #999;
  font-size: 12px;
}
</style>
