// Copyright 2026 OpenObserve Inc.

import { describe, it, expect, afterEach, vi } from "vitest";
import { mount, VueWrapper } from "@vue/test-utils";
import { h, nextTick } from "vue";
import { z } from "zod";
import OFormCheckbox from "./OFormCheckbox.vue";
import OForm from "../Form/OForm.vue";

describe("OFormCheckbox", () => {
  let wrapper: VueWrapper;

  afterEach(() => {
    wrapper?.unmount();
  });

  it("renders inside OForm without errors", () => {
    wrapper = mount(OForm, {
      props: { defaultValues: { accepted: false } },
      slots: {
        default: '<OFormCheckbox name="accepted" label="Accept terms" />',
      },
      global: {
        components: { OFormCheckbox },
      },
    });
    expect(wrapper.exists()).toBe(true);
  });

  // `validators` was removed from OFormCheckbox — required-checkbox validation
  // is now expressed via a Zod `:schema` passed to the parent OForm. Schema
  // validation (revalidateLogic "submit" mode) only runs once the form is
  // submitted, so this drives OForm's submit() rather than just toggling.
  it("shows schema validation error after submit when the checkbox is unchecked", async () => {
    const schema = z.object({
      accepted: z
        .boolean()
        .refine((v) => v === true, { message: "Required" }),
    });
    wrapper = mount(OForm, {
      props: { defaultValues: { accepted: false }, schema },
      slots: {
        default: () =>
          h(OFormCheckbox, {
            name: "accepted",
            label: "Accept terms",
          }),
      },
      global: { components: { OFormCheckbox } },
    });
    // Let the OFormCheckbox's `form.Field` finish registering before submitting.
    await nextTick();
    const vm = wrapper.vm as unknown as { submit: () => void };
    vm.submit();
    // submit() is fire-and-forget and schema validation resolves over several
    // microtask/tick hops, so poll instead of guessing a fixed number of
    // flushes (avoids flakiness when run alongside other spec files).
    await vi.waitFor(() => {
      expect(wrapper.text()).toContain("Required");
    });
  });
});
