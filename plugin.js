// TODO(bartlomieju): this should be rule name, not plugin name
const PLUGIN_NAME = "test-plugin";
const RULE1_NAME = "first-rule";

const rule = {
  create(context) {
    console.log("Hello from", `${PLUGIN_NAME}/${RULE1_NAME}`);
    context.report({
      message: "Error from " + `${PLUGIN_NAME}/${RULE1_NAME}`,
      data: {
        some: "Data",
      },
    });
    return {
      // Performs action in the function on every variable declarator
      VariableDeclarator(node) {
        // Check if a `const` variable declaration
        if (node.parent.kind === "const") {
          // Check if variable name is `foo`
          if (node.id.type === "Identifier" && node.id.name === "foo") {
            // Check if value of variable is "bar"
            if (
              node.init && node.init.type === "Literal" &&
              node.init.value !== "bar"
            ) {
              /*
               * Report error to ESLint. Error message uses
               * a message placeholder to include the incorrect value
               * in the error message.
               * Also includes a `fix(fixer)` function that replaces
               * any values assigned to `const foo` with "bar".
               */
              context.report({
                node,
                message:
                  'Value other than "bar" assigned to `const foo`. Unexpected value: {{ notBar }}.',
                data: {
                  notBar: node.init.value,
                },
                fix(fixer) {
                  return fixer.replaceText(node.init, '"bar"');
                },
              });
            }
          }
        }
      },
    };
  },
};

export default {
  name: PLUGIN_NAME,
  rules: {
    RULE1_NAME: rule,
  },
};

console.log("Loaded plugin", PLUGIN_NAME);