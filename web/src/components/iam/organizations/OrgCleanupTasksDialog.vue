<!-- Copyright 2026 OpenObserve Inc.

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
-->

<template>
  <ODialog
    :open="open"
    :title="`Deletion progress — ${orgName}`"
    size="md"
    data-test="org-cleanup-tasks-dialog"
    @update:open="$emit('update:open', $event)"
  >
    <template #body>
      <div class="tw:space-y-1">
        <!-- Header row -->
        <div class="tw:grid tw:grid-cols-[1fr_auto_auto] tw:gap-x-4 tw:text-xs tw:font-medium tw:text-text-secondary tw:px-2 tw:pb-1 tw:border-b tw:border-border-default">
          <span>Step</span>
          <span class="tw:text-right">Attempts</span>
          <span class="tw:w-20 tw:text-right">Status</span>
        </div>

        <!-- Loading -->
        <div v-if="loading && !tasks.length" class="tw:py-8 tw:text-center tw:text-text-secondary tw:text-sm">
          Loading…
        </div>

        <!-- Empty -->
        <div v-else-if="!tasks.length" class="tw:py-8 tw:text-center tw:text-text-secondary tw:text-sm">
          No cleanup tasks found for this organization.
        </div>

        <!-- Task rows -->
        <div
          v-for="task in sortedTasks"
          :key="task.id"
          class="tw:grid tw:grid-cols-[1fr_auto_auto] tw:gap-x-4 tw:items-center tw:px-2 tw:py-2 tw:rounded tw:text-sm"
          :class="{
            'tw:bg-surface-secondary': task.status === 'running',
          }"
        >
          <!-- Step name -->
          <div class="tw:flex tw:flex-col tw:gap-0.5">
            <span class="tw:font-medium tw:text-text-primary">{{ formatStepName(task.step) }}</span>
            <span v-if="task.last_error" class="tw:text-xs tw:text-error tw:truncate" :title="task.last_error">
              {{ task.last_error }}
            </span>
          </div>

          <!-- Attempts -->
          <span class="tw:text-text-secondary tw:text-xs tw:text-right tabular-nums">
            {{ task.attempts }}
          </span>

          <!-- Status badge -->
          <div class="tw:w-20 tw:flex tw:justify-end">
            <OBadge :variant="badgeVariant(task.status)" size="sm">
              {{ task.status }}
            </OBadge>
          </div>
        </div>
      </div>

      <!-- Summary footer -->
      <div v-if="tasks.length" class="tw:mt-4 tw:pt-3 tw:border-t tw:border-border-default tw:flex tw:items-center tw:justify-between tw:text-xs tw:text-text-secondary">
        <span>{{ doneCount }} / {{ tasks.length }} steps complete</span>
        <span v-if="isComplete" class="tw:text-success tw:font-medium">All done</span>
        <span v-else-if="hasFailed" class="tw:text-error">{{ failedCount }} step(s) failed permanently</span>
        <span v-else class="tw:animate-pulse">In progress — refreshing every 5s</span>
      </div>
    </template>

    <template #footer>
      <OButton variant="outline" size="sm" @click="$emit('update:open', false)">
        Close
      </OButton>
      <OButton variant="ghost" size="sm" :disabled="loading" @click="fetchTasks">
        <OIcon name="refresh" size="sm" />
        Refresh
      </OButton>
    </template>
  </ODialog>
</template>

<script lang="ts">
import { defineComponent, ref, computed, watch, onUnmounted } from "vue";
import { useStore } from "vuex";
import ODialog from "@/lib/overlay/Dialog/ODialog.vue";
import OButton from "@/lib/core/Button/OButton.vue";
import OBadge from "@/lib/core/Badge/OBadge.vue";
import OIcon from "@/lib/core/Icon/OIcon.vue";
import organizationsService from "@/services/organizations";
import type { BadgeVariant } from "@/lib/core/Badge/OBadge.types";

interface CleanupTask {
  id: string;
  org_id: string;
  org_name: string;
  step: string;
  step_order: number;
  status: string;
  attempts: number;
  last_error?: string | null;
  created_at: number;
  updated_at: number;
}

export default defineComponent({
  name: "OrgCleanupTasksDialog",
  components: { ODialog, OButton, OBadge, OIcon },
  props: {
    open: { type: Boolean, required: true },
    orgId: { type: String, required: true },
    orgName: { type: String, default: "" },
  },
  emits: ["update:open"],
  setup(props) {
    const store = useStore();
    const tasks = ref<CleanupTask[]>([]);
    const loading = ref(false);
    let pollTimer: ReturnType<typeof setInterval> | null = null;

    const metaOrg = computed(() =>
      store.state.selectedOrganization?.identifier ?? "_meta"
    );

    const sortedTasks = computed(() =>
      [...tasks.value].sort((a, b) => a.step_order - b.step_order)
    );

    const doneCount = computed(() =>
      tasks.value.filter((t) => t.status === "done").length
    );

    const failedCount = computed(() =>
      tasks.value.filter(
        (t) => t.status === "failed" && t.attempts >= 10
      ).length
    );

    const isComplete = computed(() =>
      tasks.value.length > 0 && tasks.value.every((t) => t.status === "done")
    );

    const hasFailed = computed(() => failedCount.value > 0 && isComplete.value === false);

    const fetchTasks = async () => {
      if (!props.orgId) return;
      loading.value = true;
      try {
        const res = await organizationsService.get_cleanup_tasks(
          metaOrg.value,
          props.orgId
        );
        tasks.value = res.data ?? [];
      } catch (e) {
        // silently fail — next poll will retry
      } finally {
        loading.value = false;
      }
    };

    const startPolling = () => {
      stopPolling();
      fetchTasks();
      pollTimer = setInterval(() => {
        if (!isComplete.value) fetchTasks();
      }, 5000);
    };

    const stopPolling = () => {
      if (pollTimer !== null) {
        clearInterval(pollTimer);
        pollTimer = null;
      }
    };

    watch(
      () => props.open,
      (isOpen) => {
        if (isOpen) {
          tasks.value = [];
          startPolling();
        } else {
          stopPolling();
        }
      }
    );

    onUnmounted(stopPolling);

    const formatStepName = (step: string): string => {
      // "delete_stream:logs/mystream" → "Delete stream: logs/mystream"
      if (step.includes(":")) {
        const [prefix, rest] = step.split(":", 2);
        return `${prefix.replace(/_/g, " ").replace(/\b\w/g, (c) => c.toUpperCase())}: ${rest}`;
      }
      return step
        .replace(/_/g, " ")
        .replace(/\b\w/g, (c) => c.toUpperCase());
    };

    const badgeVariant = (status: string): BadgeVariant => {
      switch (status) {
        case "done":
          return "success";
        case "running":
          return "primary";
        case "failed":
          return "error";
        default:
          return "default-outline"; // pending
      }
    };

    return {
      tasks,
      loading,
      sortedTasks,
      doneCount,
      failedCount,
      isComplete,
      hasFailed,
      fetchTasks,
      formatStepName,
      badgeVariant,
    };
  },
});
</script>
