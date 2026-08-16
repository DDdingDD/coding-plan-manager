<template>
  <div>
    <a-flex justify="space-between" align="center" style="margin-bottom: 12px">
      <a-typography-title :level="4" style="margin: 0">Coding Plan</a-typography-title>
      <a-button type="primary" @click="openCreate">新建计划</a-button>
    </a-flex>

    <a-table
      :columns="columns"
      :data-source="plans"
      :loading="loading"
      row-key="id"
      :pagination="false"
      size="middle"
    >
      <template #bodyCell="{ column, record }">
        <template v-if="column.key === 'auth_token'">
          <a-flex :gap="4" align="center">
            <a-typography-text :code="true" :copyable="false" style="max-width: 180px" :ellipsis="true">
              {{ showToken[record.id] ? record.auth_token : maskToken(record.auth_token) }}
            </a-typography-text>
            <a-button size="small" type="text" @click="showToken[record.id] = !showToken[record.id]">
              <EyeOutlined v-if="!showToken[record.id]" />
              <EyeInvisibleOutlined v-else />
            </a-button>
            <a-button size="small" type="text" @click="copyText(record.auth_token)">
              <CopyOutlined />
            </a-button>
          </a-flex>
        </template>
        <template v-else-if="column.key === 'enabled'">
          <a-switch
            :checked="record.enabled"
            @change="(v: any) => toggleEnabled(record as PlanView, v as boolean)"
          />
        </template>
        <template v-else-if="column.key === 'stats'">
          <span>{{ formatNumber(record.stats.total_tokens) }} tokens / {{ formatNumber(record.stats.requests) }} 次</span>
        </template>
        <template v-else-if="column.key === 'actions'">
          <a-flex :gap="4">
            <a-button size="small" @click="openEdit(record as PlanView)">编辑</a-button>
            <a-popconfirm title="删除后将解除所有聚合器的绑定，确定？" @confirm="removePlan(record.id)">
              <a-button size="small" danger>删除</a-button>
            </a-popconfirm>
          </a-flex>
        </template>
      </template>
    </a-table>

    <a-modal
      v-model:open="modalOpen"
      :title="editing ? '编辑计划' : '新建计划'"
      @ok="submit"
      :confirm-loading="submitting"
      destroy-on-close
    >
      <a-form layout="vertical" style="margin-top: 12px">
        <a-form-item label="名称" required>
          <a-input v-model:value="form.name" placeholder="如：GLM Coding Plan" />
        </a-form-item>
        <a-form-item label="BASE_URL" required>
          <a-input v-model:value="form.base_url" placeholder="如：https://open.bigmodel.cn/api/anthropic" />
        </a-form-item>
        <a-form-item label="AUTH_TOKEN" required>
          <a-input-password v-model:value="form.auth_token" placeholder="该计划的上游令牌" />
        </a-form-item>
        <a-form-item label="备注">
          <a-input v-model:value="form.remark" />
        </a-form-item>
      </a-form>
    </a-modal>
  </div>
</template>

<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import { message } from "ant-design-vue";
import {
  CopyOutlined,
  EyeInvisibleOutlined,
  EyeOutlined,
} from "@ant-design/icons-vue";
import { createPlan, deletePlan, listPlans, updatePlan } from "../api";
import type { PlanView } from "../types";
import { copyText, formatNumber } from "../utils";

const columns = [
  { title: "名称", dataIndex: "name", key: "name" },
  { title: "BASE_URL", dataIndex: "base_url", key: "base_url", ellipsis: true },
  { title: "AUTH_TOKEN", key: "auth_token" },
  { title: "备注", dataIndex: "remark", key: "remark", ellipsis: true },
  { title: "启用", key: "enabled", width: 80 },
  { title: "累计消耗", key: "stats", width: 200 },
  { title: "操作", key: "actions", width: 150 },
];

const plans = ref<PlanView[]>([]);
const loading = ref(false);
const showToken = reactive<Record<number, boolean>>({});

const modalOpen = ref(false);
const submitting = ref(false);
const editing = ref<PlanView | null>(null);
const form = reactive({ name: "", base_url: "", auth_token: "", remark: "" });

const maskToken = (t: string) => (t.length > 8 ? `${t.slice(0, 5)}••••••${t.slice(-4)}` : "••••");

async function refresh() {
  loading.value = true;
  try {
    plans.value = await listPlans();
  } catch (e) {
    message.error(String(e));
  } finally {
    loading.value = false;
  }
}

function openCreate() {
  editing.value = null;
  Object.assign(form, { name: "", base_url: "", auth_token: "", remark: "" });
  modalOpen.value = true;
}

function openEdit(p: PlanView) {
  editing.value = p;
  Object.assign(form, {
    name: p.name,
    base_url: p.base_url,
    auth_token: p.auth_token,
    remark: p.remark,
  });
  modalOpen.value = true;
}

async function submit() {
  submitting.value = true;
  try {
    if (editing.value) {
      await updatePlan({
        id: editing.value.id,
        name: form.name,
        baseUrl: form.base_url,
        authToken: form.auth_token,
        remark: form.remark,
        enabled: editing.value.enabled,
      });
      message.success("已更新");
    } else {
      await createPlan({
        name: form.name,
        baseUrl: form.base_url,
        authToken: form.auth_token,
        remark: form.remark,
      });
      message.success("已创建");
    }
    modalOpen.value = false;
    await refresh();
  } catch (e) {
    message.error(String(e));
  } finally {
    submitting.value = false;
  }
}

async function toggleEnabled(p: PlanView, enabled: boolean) {
  try {
    await updatePlan({
      id: p.id,
      name: p.name,
      baseUrl: p.base_url,
      authToken: p.auth_token,
      remark: p.remark,
      enabled,
    });
    await refresh();
  } catch (e) {
    message.error(String(e));
  }
}

async function removePlan(id: number) {
  try {
    await deletePlan(id);
    message.success("已删除");
    await refresh();
  } catch (e) {
    message.error(String(e));
  }
}

onMounted(refresh);
defineExpose({ refresh });
</script>
