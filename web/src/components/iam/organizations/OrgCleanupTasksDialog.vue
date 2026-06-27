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
    <!-- ODialog has no #body slot; content goes in the default slot -->
    <div>
      <!-- Progress header: overall state chip + bar -->
      <div v-if="tasks.length" class="tw:mb-4">
        <div class="tw:flex tw:items-center tw:justify-between tw:mb-2">
          <span class="tw:text-sm tw:font-medium tw:text-text-primary">
            {{ doneCount }} of {{ tasks.length }} steps complete
          </span>
          <OBadge
            :variant="overallStatus === 'completed' ? 'success-soft' : overallStatus === 'failed' ? 'error-soft' : 'primary-soft'"
            size="sm"
          >
            <OIcon
              v-if="overallStatus === 'in_progress'"
              name="autorenew"
              size="xs"
              class="tw:mr-1 tw:animate-spin"
            />
            {{ overallStatus === 'completed' ? 'Completed' : overallStatus === 'failed' ? 'Failed' : 'In progress' }}
          </OBadge>
        </div>
        <OProgressBar :value="progressValue" :variant="progressBarVariant" size="sm" />
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
      <div v-else class="tw:space-y-1.5">
        <div
          v-for="task in sortedTasks"
          :key="task.id"
          class="tw:flex tw:flex-col tw:gap-1 tw:px-3 tw:py-2 tw:rounded"
          :class="rowAccentClass(task.status)"
        >
          <div class="tw:flex tw:items-center tw:gap-3">
            <!-- Status icon -->
            <OIcon
              :name="statusIcon(task.status)"
              size="sm"
              :class="{
                'tw:text-success': task.status === 'done',
                'tw:text-primary tw:animate-spin': task.status === 'running',
                'tw:text-error': task.status === 'failed',
                'tw:text-text-secondary': task.status === 'pending',
              }"
            />

            <!-- Step name -->
            <span class="tw:flex-1 tw:font-medium tw:text-sm tw:text-text-primary tw:min-w-0 tw:truncate">
              {{ formatStepName(task.step) }}
            </span>

            <!-- Attempts (only when relevant) -->
            <span
              v-if="task.attempts > 0"
              class="tw:text-text-secondary tw:text-xs tabular-nums tw:whitespace-nowrap"
              :title="`${task.attempts} attempt(s)`"
            >
              {{ task.attempts }}×
            </span>

            <!-- Status badge -->
            <OBadge :variant="badgeVariant(task.status)" size="sm" class="tw:whitespace-nowrap">
              {{ task.status }}
            </OBadge>
          </div>

          <!-- Error message — full, wrapping, contained (no cut-off) -->
          <div
            v-if="task.last_error"
            class="tw:ml-8 tw:text-xs tw:text-error tw:bg-surface-secondary tw:rounded tw:px-2 tw:py-1 tw:break-words tw:whitespace-pre-wrap"
          >
            {{ task.last_error }}
          </div>
        </div>
      </div>

      <!-- Refresh hint while in progress -->
      <div
        v-if="tasks.length && overallStatus === 'in_progress'"
        class="tw:mt-3 tw:text-xs tw:text-text-secondary tw:flex tw:items-center tw:gap-1.5"
      >
        <OIcon name="autorenew" size="xs" class="tw:animate-spin" />
        <span>Refreshing every 5s…</span>
      </div>
      <div
        v-else-if="tasks.length && overallStatus === 'failed'"
        class="tw:mt-3 tw:text-xs tw:text-error"
      >
        {{ failedCount }} step(s) failed permanently — deletion is blocked until resolved.
      </div>
    </div>

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
import ODialog from "@/lib/overlay/Dialog/ODialog.vue";
import OButton from "@/lib/core/Button/OButton.vue";
import OBadge from "@/lib/core/Badge/OBadge.vue";
import OIcon from "@/lib/core/Icon/OIcon.vue";
import OProgressBar from "@/lib/data/ProgressBar/OProgressBar.vue";
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
  components: { ODialog, OButton, OBadge, OIcon, OProgressBar },
  props: {
    open: { type: Boolean, required: true },
    orgId: { type: String, required: true },
    orgName: { type: String, default: "" },
  },
  emits: ["update:open"],
  setup(props) {
    const tasks = ref<CleanupTask[]>([]);
    const loading = ref(false);
    let pollTimer: ReturnType<typeof setInterval> | null = null;

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

    // Progress fraction (0–1) for the bar.
    const progressValue = computed(() =>
      tasks.value.length ? doneCount.value / tasks.value.length : 0
    );

    // Overall state: completed / failed / in-progress — drives the header chip + bar colour.
    const overallStatus = computed<"completed" | "failed" | "in_progress">(() => {
      if (isComplete.value) return "completed";
      if (hasFailed.value) return "failed";
      return "in_progress";
    });

    const progressBarVariant = computed<"default" | "warning" | "danger">(() => {
      if (overallStatus.value === "completed") return "default";
      if (overallStatus.value === "failed") return "danger";
      return "default";
    });

    // Per-row left accent tint by status — gives a quick visual scan.
    const rowAccentClass = (status: string): string => {
      switch (status) {
        case "done":
          return "tw:border tw:border-border-default";
        case "running":
          return "tw:border tw:border-border-default tw:bg-surface-secondary";
        case "failed":
          return "tw:border tw:border-error";
        default:
          return "tw:border tw:border-border-default";
      }
    };

    // Status icon name per state.
    const statusIcon = (status: string): string => {
      switch (status) {
        case "done":
          return "check_circle";
        case "running":
          return "autorenew";
        case "failed":
          return "error";
        default:
          return "schedule"; // pending
      }
    };

    const fetchTasks = async () => {
      if (!props.orgId) return;
      loading.value = true;
      try {
        const res = await organizationsService.get_cleanup_tasks(props.orgId);
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
      progressValue,
      overallStatus,
      progressBarVariant,
      rowAccentClass,
      statusIcon,
      fetchTasks,
      formatStepName,
      badgeVariant,
    };
  },
});
</script>
