globalThis.DSQLEX_TEMPLATES = Object.freeze([
  Object.freeze({
    id: "field",
    label: "Field value",
    description: "Return the amount as-is",
    expression: "amount"
  }),
  Object.freeze({
    id: "arithmetic",
    label: "Arithmetic",
    description: "Double the amount, divide by five, then round",
    expression: "ROUND((amount * 2) / 5, 2)"
  }),
  Object.freeze({
    id: "simple-case",
    label: "Simple CASE",
    description: "Double Brazil amounts; leave other countries unchanged",
    expression: `CASE
    WHEN country = 'Brazil' THEN amount * 2
    ELSE amount
END`
  }),
  Object.freeze({
    id: "monster-case",
    label: "Monster CASE",
    description: "Nine country-and-amount branches with nested arithmetic",
    expression: `CASE
    WHEN country = 'Brazil' AND amount >= 700 THEN ROUND(((amount * 1.18) + ROUND(amount * 0.07, 2) - 12) / 1.10, 2)
    WHEN country = 'Brazil' AND amount >= 300 THEN ROUND(((amount * 1.12) + ROUND(amount * 0.05, 2) - 8) / 1.08, 2)
    WHEN country = 'Brazil' AND amount < 300 THEN ROUND(((amount * 1.06) + ROUND(amount * 0.03, 2) - 3) / 1.04, 2)
    WHEN country = 'Argentina' AND amount >= 700 THEN ROUND(((amount * 1.25) + ROUND(amount * 0.09, 2) - 18) / 1.14, 2)
    WHEN country = 'Argentina' AND amount >= 300 THEN ROUND(((amount * 1.17) + ROUND(amount * 0.06, 2) - 11) / 1.11, 2)
    WHEN country = 'Argentina' AND amount < 300 THEN ROUND(((amount * 1.09) + ROUND(amount * 0.04, 2) - 4) / 1.06, 2)
    WHEN country = 'Chile' AND amount >= 700 THEN ROUND(((amount * 1.22) + ROUND(amount * 0.08, 2) - 16) / 1.12, 2)
    WHEN country = 'Chile' AND amount >= 300 THEN ROUND(((amount * 1.14) + ROUND(amount * 0.05, 2) - 9) / 1.09, 2)
    WHEN country = 'Chile' AND amount < 300 THEN ROUND(((amount * 1.07) + ROUND(amount * 0.02, 2) - 2) / 1.03, 2)
    ELSE amount
END`
  })
]);
