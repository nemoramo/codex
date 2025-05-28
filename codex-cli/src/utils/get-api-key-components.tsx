import SelectInput from "../components/select-input/select-input.js";
import Spinner from "../components/vendor/ink-spinner.js";
import TextInput from "../components/vendor/ink-text-input.js";
import { Box, Text } from "ink";
import React, { useState } from "react";

export type Choice = { type: "signin" } | { type: "apikey"; key: string };

export interface ApiKeyPromptProps {
  onDone: (choice: Choice) => void;
  providerDisplayName: string;
  providerEnvKey: string;
}

export function ApiKeyPrompt({
  onDone,
  providerDisplayName,
  providerEnvKey,
}: ApiKeyPromptProps): JSX.Element {
  const [step, setStep] = useState<"select" | "paste">("select");
  const [apiKey, setApiKey] = useState("");

  const selectItems = [];
  if (providerDisplayName === "OpenAI") {
    selectItems.push({
      label: `Sign in with ${providerDisplayName}`,
      value: "signin",
    });
  }
  selectItems.push({
    label: `Paste a ${providerDisplayName} API key (or set as ${providerEnvKey})`,
    value: "paste",
  });

  if (step === "select") {
    return (
      <Box flexDirection="column" gap={1}>
        <Box flexDirection="column">
          <Text>
            {providerDisplayName === "OpenAI"
              ? `Sign in with ${providerDisplayName} to generate an API key or paste one you already have.`
              : `Paste a ${providerDisplayName} API key to continue.`}
          </Text>
          <Text dimColor>[use arrows to move, enter to select]</Text>
        </Box>
        <SelectInput
          items={selectItems}
          onSelect={(item: { value: string }) => {
            if (item.value === "signin") {
              onDone({ type: "signin" });
            } else {
              setStep("paste");
            }
          }}
        />
      </Box>
    );
  }

  return (
    <Box flexDirection="column">
      <Text>
        Paste your {providerDisplayName} API key and press &lt;Enter&gt;:
      </Text>
      <TextInput
        value={apiKey}
        onChange={setApiKey}
        onSubmit={(value: string) => {
          if (value.trim() !== "") {
            onDone({ type: "apikey", key: value.trim() });
          }
        }}
        placeholder="Enter your API key..."
        mask="*"
      />
    </Box>
  );
}

export function WaitingForAuth(): JSX.Element {
  return (
    <Box flexDirection="row" marginTop={1}>
      <Spinner type="ball" />
      <Text>
        {" "}
        Waiting for authentication… <Text dimColor>ctrl + c to quit</Text>
      </Text>
    </Box>
  );
}
