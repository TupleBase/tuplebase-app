// Seed data for the MongoDB service in docker-compose.yml. Runs once, when the data
// directory is first created, as the root user `dev`.
const app = db.getSiblingDB("app");

app.users.insertMany([
  { _id: 1, name: "Ada Lovelace", email: "ada@example.com", active: true,
    created_at: ISODate("2024-01-02T03:04:05Z"), tags: ["math", "engines"] },
  { _id: 2, name: "Edgar Codd", email: "codd@example.com", active: true,
    created_at: ISODate("2024-02-03T04:05:06Z"), address: { city: "Fortuneswell", country: "UK" } },
  { _id: 3, name: "Grace Hopper", email: null, active: false,
    created_at: ISODate("2024-03-04T05:06:07Z") },
]);
app.users.createIndex({ email: 1 }, { name: "email_1", unique: true, sparse: true });

app.orders.insertMany([
  { user_id: 1, total: NumberDecimal("19.99"), items: [{ sku: "A-1", qty: 2 }], placed: new Date("2024-05-01") },
  { user_id: 1, total: NumberDecimal("5.00"), items: [{ sku: "B-2", qty: 1 }], placed: new Date("2024-05-02") },
  { user_id: 2, total: NumberDecimal("42.00"), items: [], placed: new Date("2024-05-03") },
]);
app.orders.createIndex({ user_id: 1, placed: -1 });

app.createView("active_users", "users", [{ $match: { active: true } }, { $project: { name: 1, email: 1 } }]);

// A user who can only read `app`: the server itself refuses its writes.
db.getSiblingDB("admin").createUser({
  user: "reader",
  pwd: "reader",
  roles: [{ role: "read", db: "app" }],
});
